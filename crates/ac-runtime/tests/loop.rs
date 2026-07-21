use std::sync::Arc;

use ac_provider_mock::{MockProvider, stop_end, stop_tool_use, text, tool_use};
use ac_runtime::{AgentConfig, AgentEvent, RuntimeError, Session, StepHook};
use ac_tool::{Capability, SubtreePolicy, Tool, ToolCtx, ToolOutput, ToolRegistry};
use ac_types::StopReason;
use serde::Deserialize;
use tokio::sync::mpsc;

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
impl StepHook for SwapHook {
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
    session.add_hook(Arc::new(SwapHook));

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
impl StepHook for AppendHook {
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
    session.add_hook(Arc::new(AppendHook("-first")));
    session.add_hook(Arc::new(AppendHook("-second")));

    let (tx, _rx) = mpsc::unbounded_channel();
    session.run_turn("go".into(), tx).await.unwrap();

    let reqs = provider.requests();
    assert!(
        reqs[0].model.ends_with("-first-second"),
        "later hooks must see earlier hooks' edits: {}",
        reqs[0].model
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
