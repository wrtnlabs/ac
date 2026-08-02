use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ac_provider::{CompletionRequest, EventStream, Provider};
use ac_provider_mock::{MockProvider, stop_end, stop_tool_use, text, tool_use};
use ac_runtime::{
    AgentConfig, AgentEvent, Observation, ObservationHook, RuntimeError, Session, StepPrepareHook,
};
use ac_tool::{Capability, SubtreePolicy, Tool, ToolCtx, ToolOutput, ToolRegistry};
use ac_types::{CompletionError, CompletionEvent, ContentPart, Role, StopReason, TokenUsage};
use futures::StreamExt;
use futures::future::BoxFuture;
use serde::Deserialize;
use tokio::sync::{Notify, mpsc};

#[derive(Deserialize, schemars::JsonSchema)]
struct EchoInput {
    text: String,
}

struct Echo;

impl Tool for Echo {
    type Input = EchoInput;
    fn name(&self) -> &'static str {
        "echo"
    }
    fn description(&self) -> String {
        "echoes its text".into()
    }
    fn capability(&self) -> Capability {
        Capability::ReadOnly
    }
    fn run(
        self: Arc<Self>,
        input: Self::Input,
        _ctx: Arc<ToolCtx>,
    ) -> futures::future::BoxFuture<'static, ToolOutput> {
        Box::pin(async move { ToolOutput::ok(input.text) })
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ListFilesInput {
    #[allow(dead_code)]
    #[serde(default)]
    path: Option<String>,
}

struct ListFilesProbe {
    calls: Arc<AtomicUsize>,
}

impl Tool for ListFilesProbe {
    type Input = ListFilesInput;

    fn name(&self) -> &'static str {
        "list_files"
    }

    fn description(&self) -> String {
        "must not run for malformed JSON input".into()
    }

    fn capability(&self) -> Capability {
        Capability::ReadOnly
    }

    fn run(
        self: Arc<Self>,
        _input: Self::Input,
        _ctx: Arc<ToolCtx>,
    ) -> futures::future::BoxFuture<'static, ToolOutput> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            ToolOutput::ok("unexpected execution")
        })
    }
}

fn make_ctx() -> (Arc<ToolCtx>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let policy = SubtreePolicy::new(dir.path()).unwrap();
    let ctx = Arc::new(ToolCtx::new(Arc::new(policy)));
    (ctx, dir)
}

fn drain(mut rx: mpsc::UnboundedReceiver<AgentEvent>) -> Vec<AgentEvent> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        out.push(ev);
    }
    out
}

#[derive(Clone, Default)]
struct MalformedToolInputProvider {
    calls: Arc<AtomicUsize>,
}

impl MalformedToolInputProvider {
    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Provider for MalformedToolInputProvider {
    fn name(&self) -> &str {
        "malformed-tool-input"
    }

    fn stream_completion(
        &self,
        _request: CompletionRequest,
    ) -> BoxFuture<'static, Result<EventStream, CompletionError>> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if call == 0 {
                return Ok(futures::stream::iter([
                    Ok(CompletionEvent::ToolCallDelta {
                        id: "c_bad".to_string(),
                        name: "list_files".to_string(),
                        args_delta: r#"{"path": }"#.to_string(),
                    }),
                    Ok(CompletionEvent::InvalidToolUse {
                        id: "c_bad".to_string(),
                        name: "list_files".to_string(),
                        raw_input: r#"{"path": }"#.to_string(),
                        error: "tool input for list_files: expected value at line 1 column 10"
                            .to_string(),
                    }),
                    Ok(CompletionEvent::ToolUse(ac_types::ToolUse {
                        id: "c_good".to_string(),
                        name: "echo".to_string(),
                        input: serde_json::json!({ "text": "valid executed" }),
                    })),
                    Ok(CompletionEvent::Stop(StopReason::ToolUse)),
                ])
                .boxed());
            }
            Ok(futures::stream::iter([
                Ok(CompletionEvent::Text("recovered".to_string())),
                Ok(CompletionEvent::Stop(StopReason::EndTurn)),
            ])
            .boxed())
        })
    }
}

#[tokio::test]
async fn text_only_turn() {
    let provider = MockProvider::new(vec![vec![text("hi"), stop_end()]]);
    let (ctx, _dir) = make_ctx();
    let registry = Arc::new(ToolRegistry::new());
    let mut session = Session::new(
        Arc::new(provider.clone()),
        registry,
        ctx,
        AgentConfig::default(),
    );

    let (tx, rx) = mpsc::unbounded_channel();
    let stop = session.run_turn("hello".into(), tx).await.unwrap();
    assert!(matches!(stop, StopReason::EndTurn));
    assert_eq!(provider.call_count(), 1);

    let events = drain(rx);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Text(s) if s == "hi"))
    );
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::TurnComplete {
            stop_reason: StopReason::EndTurn
        }
    )));
}

#[tokio::test]
async fn contentless_assistant_step_is_a_turn_error() {
    let provider = MockProvider::new(vec![
        vec![
            text(" \n "),
            CompletionEvent::Thinking {
                text: "private reasoning is not an answer".to_string(),
                signature: None,
            },
            CompletionEvent::UsageUpdate(TokenUsage::default()),
            stop_end(),
        ],
        vec![stop_end()],
    ]);
    let (ctx, _dir) = make_ctx();
    let mut session = Session::new(
        Arc::new(provider.clone()),
        Arc::new(ToolRegistry::new()),
        ctx,
        AgentConfig::default(),
    );
    let (tx, rx) = mpsc::unbounded_channel();

    let error = session.run_turn("hello".into(), tx).await.unwrap_err();
    assert!(matches!(
        error,
        RuntimeError::EmptyCompletion(StopReason::EndTurn)
    ));
    assert_eq!(
        provider.call_count(),
        2,
        "the default budget retries one empty EndTurn"
    );

    let messages = session.messages();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].role, ac_types::Role::User);
    assert!(
        messages
            .iter()
            .all(|message| !ac_runtime::is_contentless(message))
    );
    assert!(
        drain(rx)
            .iter()
            .all(|event| !matches!(event, AgentEvent::TurnComplete { .. })),
        "an empty provider response must not publish successful completion"
    );
}

#[tokio::test]
async fn malformed_tool_arguments_become_a_non_executed_error_then_recover() {
    let provider = MalformedToolInputProvider::default();
    let (ctx, _dir) = make_ctx();
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    registry.register(ListFilesProbe {
        calls: tool_calls.clone(),
    });
    registry.register(Echo);
    let mut session = Session::new(
        Arc::new(provider.clone()),
        Arc::new(registry),
        ctx,
        AgentConfig::default(),
    );
    let (tx, rx) = mpsc::unbounded_channel();

    assert_eq!(
        session
            .run_turn("first attempt".to_string(), tx)
            .await
            .expect("the model gets a corrective step"),
        StopReason::EndTurn
    );
    assert_eq!(
        provider.call_count(),
        2,
        "the error result must be followed by a corrective sampling step"
    );
    assert_eq!(
        tool_calls.load(Ordering::SeqCst),
        0,
        "malformed JSON must never reach tool execution"
    );

    let events = drain(rx);
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolInputDelta { id, name, delta }
            if id == "c_bad" && name == "list_files" && delta == r#"{"path": }"#
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolInputError {
            id,
            name,
            input,
            error,
        }
            if id == "c_bad"
                && name == "list_files"
                && input == &serde_json::json!(r#"{"path": }"#)
                && error.contains("expected value at line 1 column 10")
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolCall { id, name, input }
            if id == "c_good"
                && name == "echo"
                && input == &serde_json::json!({ "text": "valid executed" })
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolResult { id, name, output, is_error }
            if id == "c_bad"
                && name == "list_files"
                && *is_error
                && output.contains("expected value at line 1 column 10")
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolResult { id, name, output, is_error }
            if id == "c_good"
                && name == "echo"
                && !*is_error
                && output == "valid executed"
    )));
    let call_positions = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| match event {
            AgentEvent::ToolInputError { id, .. } | AgentEvent::ToolCall { id, .. } => {
                Some((id.as_str(), index))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let result_positions = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| match event {
            AgentEvent::ToolResult { id, .. } => Some((id.as_str(), index)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        call_positions.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        ["c_bad", "c_good"]
    );
    assert_eq!(
        result_positions
            .iter()
            .map(|(id, _)| *id)
            .collect::<Vec<_>>(),
        ["c_bad", "c_good"]
    );
    assert!(
        call_positions.last().unwrap().1 < result_positions.first().unwrap().1,
        "all calls must precede all results"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::Text(text) if text == "recovered"))
    );
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::TurnComplete {
            stop_reason: StopReason::EndTurn
        }
    )));

    let recovered = session.messages();
    assert_eq!(
        recovered
            .iter()
            .map(|message| message.role)
            .collect::<Vec<_>>(),
        [Role::User, Role::Assistant, Role::User, Role::Assistant]
    );
    assert!(matches!(
        recovered[0].content.as_slice(),
        [ContentPart::Text { text }] if text == "first attempt"
    ));
    assert!(matches!(
        recovered[1].content.as_slice(),
        [ContentPart::ToolUse(invalid), ContentPart::ToolUse(valid)]
            if invalid.id == "c_bad"
                && invalid.name == "list_files"
                && invalid.input == serde_json::json!({})
                && valid.id == "c_good"
                && valid.name == "echo"
                && valid.input == serde_json::json!({ "text": "valid executed" })
    ));
    assert!(matches!(
        recovered[2].content.as_slice(),
        [ContentPart::ToolResult(invalid), ContentPart::ToolResult(valid)]
            if invalid.tool_use_id == "c_bad"
                && invalid.is_error
                && invalid.content.contains("expected value at line 1 column 10")
                && valid.tool_use_id == "c_good"
                && !valid.is_error
                && valid.content == "valid executed"
    ));
    assert!(matches!(
        recovered[3].content.as_slice(),
        [ContentPart::Text { text }] if text == "recovered"
    ));
}

#[tokio::test]
async fn one_contentless_end_turn_is_retried_successfully_by_default() {
    let provider = MockProvider::new(vec![vec![stop_end()], vec![text("recovered"), stop_end()]]);
    let (ctx, _dir) = make_ctx();
    let mut session = Session::new(
        Arc::new(provider.clone()),
        Arc::new(ToolRegistry::new()),
        ctx,
        AgentConfig::default(),
    );
    let (tx, rx) = mpsc::unbounded_channel();

    assert_eq!(
        session.run_turn("hello".into(), tx).await.unwrap(),
        StopReason::EndTurn
    );
    assert_eq!(provider.call_count(), 2);
    assert!(session.messages().iter().any(|message| {
        message.role == ac_types::Role::Assistant
            && message.content.iter().any(
                |part| matches!(part, ac_types::ContentPart::Text { text } if text == "recovered"),
            )
    }));
    assert_eq!(
        drain(rx)
            .iter()
            .filter(|event| matches!(event, AgentEvent::TurnComplete { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn contentless_end_turn_retry_budget_is_configurable() {
    let provider = MockProvider::new(vec![
        vec![stop_end()],
        vec![text("must not be sampled"), stop_end()],
    ]);
    let (ctx, _dir) = make_ctx();
    let mut session = Session::new(
        Arc::new(provider.clone()),
        Arc::new(ToolRegistry::new()),
        ctx,
        AgentConfig {
            empty_completion_retries: 0,
            ..AgentConfig::default()
        },
    );
    let (tx, _rx) = mpsc::unbounded_channel();

    assert!(matches!(
        session.run_turn("hello".into(), tx).await.unwrap_err(),
        RuntimeError::EmptyCompletion(StopReason::EndTurn)
    ));
    assert_eq!(provider.call_count(), 1);
}

#[tokio::test]
async fn contentless_follow_up_after_tool_results_is_a_turn_error() {
    let provider = MockProvider::new(vec![
        vec![
            tool_use("c1", "echo", serde_json::json!({"text": "yo"})),
            stop_tool_use(),
        ],
        vec![stop_end()],
        vec![stop_end()],
    ]);
    let (ctx, _dir) = make_ctx();
    let mut registry = ToolRegistry::new();
    registry.register(Echo);
    let mut session = Session::new(
        Arc::new(provider),
        Arc::new(registry),
        ctx,
        AgentConfig::default(),
    );
    let (tx, rx) = mpsc::unbounded_channel();

    let error = session.run_turn("go".into(), tx).await.unwrap_err();
    assert!(matches!(
        error,
        RuntimeError::EmptyCompletion(StopReason::EndTurn)
    ));
    let messages = session.messages();
    assert_eq!(
        messages
            .iter()
            .map(|message| message.role)
            .collect::<Vec<_>>(),
        [
            ac_types::Role::User,
            ac_types::Role::Assistant,
            ac_types::Role::User
        ],
        "the completed call/result pair remains valid history"
    );
    assert!(matches!(
        messages[1].content.as_slice(),
        [ac_types::ContentPart::ToolUse(_)]
    ));
    assert!(matches!(
        messages[2].content.as_slice(),
        [ac_types::ContentPart::ToolResult(_)]
    ));
    assert!(
        drain(rx)
            .iter()
            .all(|event| !matches!(event, AgentEvent::TurnComplete { .. }))
    );
}

#[tokio::test]
async fn tool_loop() {
    let provider = MockProvider::new(vec![
        vec![
            tool_use("c1", "echo", serde_json::json!({"text": "yo"})),
            stop_tool_use(),
        ],
        vec![text("done"), stop_end()],
    ]);
    let (ctx, _dir) = make_ctx();
    let mut registry = ToolRegistry::new();
    registry.register(Echo);
    let mut session = Session::new(
        Arc::new(provider.clone()),
        Arc::new(registry),
        ctx,
        AgentConfig::default(),
    );

    let (tx, rx) = mpsc::unbounded_channel();
    let stop = session.run_turn("go".into(), tx).await.unwrap();
    assert!(matches!(stop, StopReason::EndTurn));
    assert_eq!(provider.call_count(), 2);

    // Second request carries the tool result for c1.
    let reqs = provider.requests();
    let second = &reqs[1];
    let has_result = second.messages.iter().any(|m| {
        m.content
            .iter()
            .any(|p| matches!(p, ac_types::ContentPart::ToolResult(tr) if tr.tool_use_id == "c1"))
    });
    assert!(
        has_result,
        "second request must contain the ToolResult for c1"
    );

    // Ordering of emitted events: ToolCall then ToolResult then Text.
    let events = drain(rx);
    let call_idx = events
        .iter()
        .position(|e| matches!(e, AgentEvent::ToolCall { id, .. } if id == "c1"))
        .unwrap();
    let result_idx = events
        .iter()
        .position(|e| matches!(e, AgentEvent::ToolResult { id, .. } if id == "c1"))
        .unwrap();
    let text_idx = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Text(s) if s == "done"))
        .unwrap();
    assert!(call_idx < result_idx && result_idx < text_idx);
}

#[tokio::test]
async fn unknown_tool() {
    let provider = MockProvider::new(vec![
        vec![
            tool_use("c1", "nope", serde_json::json!({})),
            stop_tool_use(),
        ],
        vec![text("recovered"), stop_end()],
    ]);
    let (ctx, _dir) = make_ctx();
    let registry = Arc::new(ToolRegistry::new());
    let mut session = Session::new(
        Arc::new(provider.clone()),
        registry,
        ctx,
        AgentConfig::default(),
    );

    let (tx, rx) = mpsc::unbounded_channel();
    let stop = session.run_turn("go".into(), tx).await.unwrap();
    assert!(matches!(stop, StopReason::EndTurn));

    let events = drain(rx);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolResult { is_error: true, .. }))
    );
}

struct HideEcho;
impl StepPrepareHook for HideEcho {
    fn prepare(&self, _iteration: usize, request: &mut ac_provider::CompletionRequest) {
        request.tools.retain(|tool| tool.name != "echo");
    }
}

#[tokio::test]
async fn a_registered_but_unoffered_tool_cannot_bypass_the_step_gate() {
    let provider = MockProvider::new(vec![
        vec![
            tool_use("c1", "echo", serde_json::json!({"text": "must not run"})),
            stop_tool_use(),
        ],
        vec![text("recovered"), stop_end()],
    ]);
    let (ctx, _dir) = make_ctx();
    let mut registry = ToolRegistry::new();
    registry.register(Echo);
    let mut session = Session::new(
        Arc::new(provider),
        Arc::new(registry),
        ctx,
        AgentConfig::default(),
    );
    session.add_step_hook(Arc::new(HideEcho));

    let (tx, rx) = mpsc::unbounded_channel();
    session.run_turn("go".into(), tx).await.unwrap();
    let events = drain(rx);
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolResult {
            name,
            output,
            is_error: true,
            ..
        } if name == "echo" && output.contains("not available on this step")
    )));
}

#[tokio::test]
async fn max_iterations() {
    let provider = MockProvider::new(vec![
        vec![
            tool_use("c1", "echo", serde_json::json!({"text": "a"})),
            stop_tool_use(),
        ],
        vec![
            tool_use("c2", "echo", serde_json::json!({"text": "b"})),
            stop_tool_use(),
        ],
        vec![
            tool_use("c3", "echo", serde_json::json!({"text": "c"})),
            stop_tool_use(),
        ],
    ]);
    let (ctx, _dir) = make_ctx();
    let mut registry = ToolRegistry::new();
    registry.register(Echo);
    let config = AgentConfig {
        max_iterations: 2,
        ..Default::default()
    };
    let mut session = Session::new(Arc::new(provider), Arc::new(registry), ctx, config);

    let (tx, _rx) = mpsc::unbounded_channel();
    let err = session.run_turn("go".into(), tx).await.unwrap_err();
    assert!(matches!(err, RuntimeError::MaxIterations(2)));
}

struct SwapHook;
impl StepPrepareHook for SwapHook {
    fn prepare(&self, iteration: usize, request: &mut ac_provider::CompletionRequest) {
        if iteration == 0 {
            request.model = "swapped".into();
            request.tool_choice = ac_provider::ToolChoice::Force("echo".into());
        }
    }
}

#[tokio::test]
async fn step_hook() {
    let provider = MockProvider::new(vec![vec![text("hi"), stop_end()]]);
    let (ctx, _dir) = make_ctx();
    let mut registry = ToolRegistry::new();
    registry.register(Echo);
    let mut session = Session::new(
        Arc::new(provider.clone()),
        Arc::new(registry),
        ctx,
        AgentConfig::default(),
    );
    session.add_step_hook(Arc::new(SwapHook));

    let (tx, _rx) = mpsc::unbounded_channel();
    session.run_turn("go".into(), tx).await.unwrap();

    let reqs = provider.requests();
    assert_eq!(reqs[0].model, "swapped");
    assert!(matches!(
        reqs[0].tool_choice,
        ac_provider::ToolChoice::Force(ref n) if n == "echo"
    ));
}

/// Hooks compose in registration order, each seeing the previous edits.
struct AppendHook(&'static str);
impl StepPrepareHook for AppendHook {
    fn prepare(&self, _iteration: usize, request: &mut ac_provider::CompletionRequest) {
        let mut model = request.model.clone();
        model.push_str(self.0);
        request.model = model;
    }
}

#[tokio::test]
async fn hooks_compose_in_registration_order() {
    let provider = MockProvider::new(vec![vec![text("hi"), stop_end()]]);
    let (ctx, _dir) = make_ctx();
    let mut session = Session::new(
        Arc::new(provider.clone()),
        Arc::new(ToolRegistry::new()),
        ctx,
        AgentConfig::default(),
    );
    session.add_step_hook(Arc::new(AppendHook("-first")));
    session.add_step_hook(Arc::new(AppendHook("-second")));

    let (tx, _rx) = mpsc::unbounded_channel();
    session.run_turn("go".into(), tx).await.unwrap();

    let reqs = provider.requests();
    assert!(
        reqs[0].model.ends_with("-first-second"),
        "later hooks must see earlier hooks' edits: {}",
        reqs[0].model
    );
}

/// The observation phase fires at tool start and finish, in dispatch order, and
/// sees the outcome — while contributing nothing model-visible (I4/I6).
#[tokio::test]
async fn observation_hooks_see_tool_traffic() {
    use std::sync::Mutex;

    struct Recorder(Arc<Mutex<Vec<String>>>);
    impl ObservationHook for Recorder {
        fn observe(&self, event: &Observation) {
            let line = match event {
                Observation::ToolStart { name, .. } => format!("start:{name}"),
                Observation::ToolFinish { name, is_error, .. } => {
                    format!("finish:{name}:{is_error}")
                }
            };
            self.0.lock().unwrap().push(line);
        }
    }

    let provider = MockProvider::new(vec![
        vec![
            tool_use("c1", "echo", serde_json::json!({ "text": "hi" })),
            stop_tool_use(),
        ],
        vec![text("done"), stop_end()],
    ]);
    let (ctx, _dir) = make_ctx();
    let mut registry = ToolRegistry::new();
    registry.register(Echo);
    let mut session = Session::new(
        Arc::new(provider.clone()),
        Arc::new(registry),
        ctx,
        AgentConfig::default(),
    );
    let log = Arc::new(Mutex::new(Vec::new()));
    session.add_observation_hook(Arc::new(Recorder(log.clone())));

    let (tx, _rx) = mpsc::unbounded_channel();
    session.run_turn("go".into(), tx).await.unwrap();

    assert_eq!(
        *log.lock().unwrap(),
        vec!["start:echo".to_string(), "finish:echo:false".to_string()],
        "the observer must see the echo call start then finish successfully"
    );
}

#[tokio::test]
async fn cancellation() {
    let provider = MockProvider::new(vec![vec![text("hi"), stop_end()]]);
    let (ctx, _dir) = make_ctx();
    ctx.cancel.cancel();
    let registry = Arc::new(ToolRegistry::new());
    let mut session = Session::new(Arc::new(provider), registry, ctx, AgentConfig::default());

    let (tx, _rx) = mpsc::unbounded_channel();
    let err = session.run_turn("go".into(), tx).await.unwrap_err();
    assert!(matches!(err, RuntimeError::Cancelled));
}

struct Blocking {
    started: Arc<Notify>,
}

impl Tool for Blocking {
    type Input = EchoInput;

    fn name(&self) -> &'static str {
        "blocking"
    }

    fn description(&self) -> String {
        "waits forever".into()
    }

    fn capability(&self) -> Capability {
        Capability::ReadOnly
    }

    fn run(
        self: Arc<Self>,
        _input: Self::Input,
        _ctx: Arc<ToolCtx>,
    ) -> futures::future::BoxFuture<'static, ToolOutput> {
        Box::pin(async move {
            self.started.notify_one();
            std::future::pending().await
        })
    }
}

#[tokio::test]
async fn cancellation_while_a_tool_runs_closes_the_call_in_history() {
    let provider = MockProvider::new(vec![vec![
        tool_use("c1", "blocking", serde_json::json!({"text": "x"})),
        stop_tool_use(),
    ]]);
    let (ctx, _dir) = make_ctx();
    let cancel = ctx.cancel.clone();
    let started = Arc::new(Notify::new());
    let mut registry = ToolRegistry::new();
    registry.register(Blocking {
        started: started.clone(),
    });
    let mut session = Session::new(
        Arc::new(provider),
        Arc::new(registry),
        ctx,
        AgentConfig::default(),
    );
    let (tx, _rx) = mpsc::unbounded_channel();

    let running = tokio::spawn(async move {
        let result = session.run_turn("go".into(), tx).await;
        (session, result)
    });
    started.notified().await;
    cancel.cancel();
    let (session, result) = tokio::time::timeout(std::time::Duration::from_secs(1), running)
        .await
        .expect("cancellation must not wait for the tool")
        .unwrap();
    assert!(matches!(result, Err(RuntimeError::Cancelled)));

    let messages = session.messages();
    let call_index = messages
        .iter()
        .position(|message| {
            message
                .content
                .iter()
                .any(|part| matches!(part, ac_types::ContentPart::ToolUse(call) if call.id == "c1"))
        })
        .unwrap();
    let result_index = messages
        .iter()
        .position(|message| {
            message.content.iter().any(|part| {
                matches!(
                    part,
                    ac_types::ContentPart::ToolResult(result)
                        if result.tool_use_id == "c1"
                            && result.is_error
                            && result.content == ac_runtime::ABORTED_TOOL_RESULT
                )
            })
        })
        .unwrap();
    assert_eq!(result_index, call_index + 1);
}

/// A tool that panics. Its `run` future unwinds; the runtime must catch that
/// (via task isolation) and still produce exactly one tool_result, so the
/// message history stays valid and the loop can continue.
struct Panics;
impl Tool for Panics {
    type Input = EchoInput;
    fn name(&self) -> &'static str {
        "panics"
    }
    fn description(&self) -> String {
        "panics on purpose".into()
    }
    fn capability(&self) -> Capability {
        Capability::ReadOnly
    }
    fn run(
        self: Arc<Self>,
        _input: Self::Input,
        _ctx: Arc<ToolCtx>,
    ) -> futures::future::BoxFuture<'static, ToolOutput> {
        Box::pin(async move { panic!("boom") })
    }
}

#[tokio::test]
async fn panicking_tool_becomes_error_result_and_turn_continues() {
    let provider = MockProvider::new(vec![
        vec![
            tool_use("c1", "panics", serde_json::json!({"text": "x"})),
            stop_tool_use(),
        ],
        vec![text("survived"), stop_end()],
    ]);
    let (ctx, _dir) = make_ctx();
    let mut registry = ToolRegistry::new();
    registry.register(Panics);
    let mut session = Session::new(
        Arc::new(provider.clone()),
        Arc::new(registry),
        ctx,
        AgentConfig::default(),
    );

    let (tx, rx) = mpsc::unbounded_channel();
    // The turn must NOT unwind — it recovers and reaches EndTurn.
    let stop = session.run_turn("go".into(), tx).await.unwrap();
    assert!(matches!(stop, StopReason::EndTurn));
    assert_eq!(provider.call_count(), 2);

    // The panic surfaced as an error tool_result...
    let events = drain(rx);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolResult { id, is_error: true, .. } if id == "c1")),
        "panicking tool must yield an error tool_result"
    );
    // ...and the second request carries a ToolResult for c1, so history is valid
    // (an assistant tool_use with no matching tool_result would 400 the model).
    let second = &provider.requests()[1];
    assert!(
        second
            .messages
            .iter()
            .any(|m| m.content.iter().any(
                |p| matches!(p, ac_types::ContentPart::ToolResult(tr) if tr.tool_use_id == "c1")
            )),
        "every tool_use must be answered by a tool_result"
    );
}

/// If the event receiver is dropped, the loop should stop rather than keep
/// spending tokens and running tools for nobody.
#[tokio::test]
async fn dropped_receiver_stops_the_loop() {
    let provider = MockProvider::new(vec![
        vec![
            tool_use("c1", "echo", serde_json::json!({"text": "a"})),
            stop_tool_use(),
        ],
        vec![text("done"), stop_end()],
    ]);
    let (ctx, _dir) = make_ctx();
    let mut registry = ToolRegistry::new();
    registry.register(Echo);
    let mut session = Session::new(
        Arc::new(provider.clone()),
        Arc::new(registry),
        ctx,
        AgentConfig::default(),
    );

    let (tx, rx) = mpsc::unbounded_channel();
    drop(rx); // nobody is listening
    let err = session.run_turn("go".into(), tx).await.unwrap_err();
    assert!(matches!(err, RuntimeError::Cancelled));
    // Stopped immediately: no model round-trip was even issued.
    assert_eq!(provider.call_count(), 0);
}
