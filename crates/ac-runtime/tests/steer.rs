//! Integration proof of mid-turn steering ([docs/ac-queue-steer.md]) through
//! the real run loop: a tool steers while the turn runs, and the drain reaches
//! the next model request; cancellation records the interruption marker and
//! discards the queue.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use ac_provider::{CompletionRequest, EventStream, Provider};
use ac_provider_mock::{MockProvider, stop_end, stop_tool_use, text, tool_use};
use ac_runtime::{AgentConfig, AgentEvent, INTERRUPTION_MARKER, RuntimeError, Session, SteerInput};
use ac_tool::{Capability, SubtreePolicy, Tool, ToolCtx, ToolOutput, ToolRegistry};
use ac_types::{CompletionError, CompletionEvent, ContentPart, Message, Role, StopReason};
use futures::StreamExt;
use futures::future::BoxFuture;
use serde::Deserialize;
use tokio::sync::mpsc;

fn make_ctx() -> (Arc<ToolCtx>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let policy = SubtreePolicy::new(dir.path()).unwrap();
    let ctx = Arc::new(ToolCtx::new(Arc::new(policy)));
    (ctx, dir)
}

/// A tool that, when the model calls it, steers a fixed message into the
/// running turn — a deterministic mid-turn steer at a known point (during the
/// first step's tool execution), so the drain must appear at the next step's
/// boundary. It reaches the session's steer handle through a shared cell set
/// after the session is constructed.
#[derive(Clone)]
struct Steerer {
    handle: Arc<OnceLock<ac_runtime::SteerHandle>>,
    message: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct Empty {}

impl Tool for Steerer {
    type Input = Empty;
    fn name(&self) -> &'static str {
        "steerer"
    }
    fn description(&self) -> String {
        "steers a message into the running turn".into()
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
            let handle = self.handle.get().expect("steer handle set");
            handle
                .steer(vec![SteerInput::text(self.message.clone())])
                .expect("steer accepted mid-turn");
            ToolOutput::ok("steered")
        })
    }
}

async fn run(
    mut session: Session,
    prompt: &str,
) -> (Result<StopReason, RuntimeError>, Vec<AgentEvent>) {
    let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();
    let prompt = prompt.to_string();
    let driver = tokio::spawn(async move { session.run_turn(prompt, tx).await });
    let mut events = Vec::new();
    while let Some(ev) = rx.recv().await {
        events.push(ev);
    }
    (driver.await.expect("join"), events)
}

#[tokio::test]
async fn a_steer_during_a_tool_call_is_sampled_in_the_next_step() {
    // Step 1 calls the steerer; step 2 answers. The steer submitted during
    // step 1's tool run must be drained at step 2's boundary and appear in the
    // second request's messages.
    let provider = MockProvider::new(vec![
        vec![
            tool_use("call-steer", "steerer", serde_json::json!({})),
            stop_tool_use(),
        ],
        vec![text("done"), stop_end()],
    ]);
    let handle_cell = Arc::new(OnceLock::new());
    let mut registry = ToolRegistry::new();
    registry.register(Steerer {
        handle: handle_cell.clone(),
        message: "STEERED-9137".to_string(),
    });

    let (ctx, _dir) = make_ctx();
    let handle_provider = provider.clone();
    let session = Session::new(
        Arc::new(provider),
        Arc::new(registry),
        ctx,
        AgentConfig::default(),
    );
    handle_cell.set(session.steer_handle()).ok().unwrap();

    let (result, events) = run(session, "begin").await;
    assert_eq!(result.expect("turn ok"), StopReason::EndTurn);

    let requests = handle_provider.requests();
    assert_eq!(requests.len(), 2, "two model round-trips");

    // The first request must NOT contain the steer (it hadn't happened yet):
    // the turn's own input samples before any steer.
    let in_first = requests[0].messages.iter().any(|m| {
        m.content
            .iter()
            .any(|p| matches!(p, ContentPart::Text { text } if text.contains("STEERED-9137")))
    });
    assert!(
        !in_first,
        "the steer must not appear before it was submitted"
    );

    // The second request must contain it, as a plain user message positioned
    // after the tool result (no wrapper text — R3 neutrality).
    let steer_msg = requests[1].messages.iter().find(|m| {
        m.role == Role::User
            && m.content
                .iter()
                .any(|p| matches!(p, ContentPart::Text { text } if text == "STEERED-9137"))
    });
    assert!(
        steer_msg.is_some(),
        "the steer must be drained into the next request verbatim: {:?}",
        requests[1].messages
    );

    let result_index = events
        .iter()
        .position(|event| {
            matches!(
                event,
                AgentEvent::ToolResult {
                    id,
                    is_error: false,
                    ..
                } if id == "call-steer"
            )
        })
        .expect("tool result event");
    let committed_index = events
        .iter()
        .position(|event| {
            matches!(
                event,
                AgentEvent::InputCommitted { message }
                    if message == &Message::text(Role::User, "STEERED-9137")
            )
        })
        .expect("input checkpoint event");
    let continuation_index = events
        .iter()
        .position(|event| matches!(event, AgentEvent::Text(text) if text == "done"))
        .expect("continuation text event");
    assert!(
        result_index < committed_index && committed_index < continuation_index,
        "the input checkpoint belongs exactly between completed steps: {events:?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEvent::InputCommitted { .. }))
            .count(),
        1,
        "initial input and runtime-authored tool-result rows must not emit checkpoints"
    );
}

/// Steers a fixed message exactly once, on iteration 0 — a deterministic
/// injection on the turn's own thread (a hook runs synchronously at each step
/// boundary), so the steer is provably pending when the first text-only step
/// finishes.
struct SteerOnceHook {
    handle: Arc<OnceLock<ac_runtime::SteerHandle>>,
    fired: std::sync::atomic::AtomicBool,
    message: String,
}

#[derive(Clone, Default)]
struct FailAfterFirstResponse {
    calls: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
}

impl FailAfterFirstResponse {
    fn requests(&self) -> Vec<CompletionRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl Provider for FailAfterFirstResponse {
    fn name(&self) -> &str {
        "fail-after-first-response"
    }

    fn stream_completion(
        &self,
        request: CompletionRequest,
    ) -> BoxFuture<'static, Result<EventStream, CompletionError>> {
        self.requests.lock().unwrap().push(request);
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if call > 0 {
                return Err(CompletionError::BadRequest(
                    "scripted failure after completed response".to_string(),
                ));
            }
            Ok(futures::stream::iter([
                Ok(CompletionEvent::Text("before steer".to_string())),
                Ok(CompletionEvent::Stop(StopReason::EndTurn)),
            ])
            .boxed())
        })
    }
}

impl ac_runtime::StepPrepareHook for SteerOnceHook {
    fn prepare(&self, iteration: usize, _request: &mut ac_provider::CompletionRequest) {
        use std::sync::atomic::Ordering;
        if iteration == 0 && !self.fired.swap(true, Ordering::SeqCst) {
            self.handle
                .get()
                .expect("handle set")
                .steer(vec![SteerInput::text(self.message.clone())])
                .expect("steer accepted");
        }
    }
}

#[tokio::test]
async fn a_pending_steer_extends_an_otherwise_final_text_step() {
    // Step 0 is text-only — normally the turn would end — but a steer is
    // pending (injected at iteration 0), so continuation `follow_up ∨ Q≠∅`
    // keeps the turn alive and samples the steer in step 1.
    let provider = MockProvider::new(vec![
        vec![text("first"), stop_end()],
        vec![text("second"), stop_end()],
    ]);
    let (ctx, _dir) = make_ctx();
    let handle_provider = provider.clone();
    let handle_cell = Arc::new(OnceLock::new());
    let mut session = Session::new(
        Arc::new(provider),
        Arc::new(ToolRegistry::new()),
        ctx,
        AgentConfig::default(),
    );
    handle_cell.set(session.steer_handle()).ok().unwrap();
    session.add_step_hook(Arc::new(SteerOnceHook {
        handle: handle_cell,
        fired: std::sync::atomic::AtomicBool::new(false),
        message: "MORE-4423".to_string(),
    }));

    let (result, _events) = run(session, "go").await;
    assert_eq!(result.expect("turn ok"), StopReason::EndTurn);

    let requests = handle_provider.requests();
    assert_eq!(requests.len(), 2, "the pending steer extended the turn");
    assert!(
        requests[1].messages.iter().any(|m| m
            .content
            .iter()
            .any(|p| matches!(p, ContentPart::Text { text } if text == "MORE-4423"))),
        "the steer must be sampled in the extending step",
    );
}

#[tokio::test]
async fn completed_assistant_and_drained_steer_survive_later_provider_error() {
    let provider = FailAfterFirstResponse::default();
    let (ctx, _dir) = make_ctx();
    let handle_cell = Arc::new(OnceLock::new());
    let mut session = Session::new(
        Arc::new(provider.clone()),
        Arc::new(ToolRegistry::new()),
        ctx,
        AgentConfig::default(),
    );
    handle_cell.set(session.steer_handle()).ok().unwrap();
    session.add_step_hook(Arc::new(SteerOnceHook {
        handle: handle_cell,
        fired: std::sync::atomic::AtomicBool::new(false),
        message: "CHANGE-DIRECTION-4423".to_string(),
    }));
    let (tx, mut rx) = mpsc::unbounded_channel();

    let error = session.run_turn("go".into(), tx).await.unwrap_err();
    assert!(matches!(error, RuntimeError::Completion(_)));

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1].messages.iter().any(|message| {
            message.role == Role::User
                && message.content.iter().any(|part| {
                    matches!(
                        part,
                        ContentPart::Text { text } if text == "CHANGE-DIRECTION-4423"
                    )
                })
        }),
        "the steer must be sampled by the failing follow-up request"
    );

    let messages = session.messages();
    assert_eq!(
        messages
            .iter()
            .map(|message| message.role)
            .collect::<Vec<_>>(),
        [Role::User, Role::Assistant, Role::User]
    );
    assert!(matches!(
        messages[1].content.as_slice(),
        [ContentPart::Text { text }] if text == "before steer"
    ));
    assert!(matches!(
        messages[2].content.as_slice(),
        [ContentPart::Text { text }] if text == "CHANGE-DIRECTION-4423"
    ));
    while let Ok(event) = rx.try_recv() {
        assert!(
            !matches!(event, AgentEvent::TurnComplete { .. }),
            "a failed follow-up request must not publish successful completion"
        );
    }
}

#[tokio::test]
async fn terminal_flush_emits_input_checkpoint_before_failure_framing() {
    // Queue a steer before the initial request. Initial deferral prevents it
    // from being sampled; the request then fails, so the non-cancel terminal
    // flush is the only path that can durably commit it.
    let provider = FailAfterFirstResponse {
        calls: Arc::new(AtomicUsize::new(1)),
        ..Default::default()
    };
    let (ctx, _dir) = make_ctx();
    let handle_cell = Arc::new(OnceLock::new());
    let mut session = Session::new(
        Arc::new(provider),
        Arc::new(ToolRegistry::new()),
        ctx,
        AgentConfig::default(),
    );
    handle_cell.set(session.steer_handle()).ok().unwrap();
    session.add_step_hook(Arc::new(SteerOnceHook {
        handle: handle_cell,
        fired: std::sync::atomic::AtomicBool::new(false),
        message: "TERMINAL-FLUSH-4423".to_string(),
    }));

    let (tx, mut rx) = mpsc::unbounded_channel();
    let host_sink = tx.clone();
    let error = session.run_turn("go".into(), tx).await.unwrap_err();
    assert!(matches!(error, RuntimeError::Completion(_)));
    host_sink
        .send(AgentEvent::Error(error.to_string()))
        .expect("host failure framing");
    drop(host_sink);

    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    assert!(matches!(
        events.as_slice(),
        [
            AgentEvent::InputCommitted { message },
            AgentEvent::Error(_)
        ] if message == &Message::text(Role::User, "TERMINAL-FLUSH-4423")
    ));
    assert_eq!(
        session.messages(),
        [
            Message::text(Role::User, "go"),
            Message::text(Role::User, "TERMINAL-FLUSH-4423"),
        ],
        "the checkpoint reports the exact row already appended by terminal flush"
    );
}

#[tokio::test]
async fn a_drained_steer_is_preserved_when_the_provider_completes_empty() {
    let provider = MockProvider::new(vec![
        vec![text("first"), stop_end()],
        vec![stop_end()],
        vec![stop_end()],
    ]);
    let (ctx, _dir) = make_ctx();
    let handle_cell = Arc::new(OnceLock::new());
    let mut session = Session::new(
        Arc::new(provider),
        Arc::new(ToolRegistry::new()),
        ctx,
        AgentConfig::default(),
    );
    handle_cell.set(session.steer_handle()).ok().unwrap();
    session.add_step_hook(Arc::new(SteerOnceHook {
        handle: handle_cell,
        fired: std::sync::atomic::AtomicBool::new(false),
        message: "MORE-EMPTY-4423".to_string(),
    }));
    let (tx, mut rx) = mpsc::unbounded_channel();

    let error = session.run_turn("go".into(), tx).await.unwrap_err();
    assert!(matches!(
        error,
        RuntimeError::EmptyCompletion(StopReason::EndTurn)
    ));
    assert!(
        session.messages().iter().any(|message| {
            message.role == Role::User
                && message.content.iter().any(
                    |part| matches!(part, ContentPart::Text { text } if text == "MORE-EMPTY-4423"),
                )
        }),
        "a steer drained before the empty sample remains in valid history"
    );
    while let Ok(event) = rx.try_recv() {
        assert!(
            !matches!(event, AgentEvent::TurnComplete { .. }),
            "empty completion must not publish successful completion"
        );
    }
}

#[tokio::test]
async fn cancellation_records_the_marker_and_discards_pending_steers() {
    let provider = MockProvider::new(vec![vec![text("hi"), stop_end()]]);
    let (ctx, _dir) = make_ctx();
    ctx.cancel.cancel();
    let mut session = Session::new(
        Arc::new(provider),
        Arc::new(ToolRegistry::new()),
        ctx,
        AgentConfig::default(),
    );
    // A steer queued before the (already-cancelled) turn is discarded by cancel.
    let handle = session.steer_handle();

    let (tx, _rx) = mpsc::unbounded_channel::<AgentEvent>();
    let err = session.run_turn("go".into(), tx).await.unwrap_err();
    assert!(matches!(err, RuntimeError::Cancelled));

    // The interruption marker is the last message, so the next turn's model
    // reads the cut as deliberate.
    let messages = session.messages();
    let last = messages.last().expect("a message");
    assert_eq!(last.role, Role::User);
    assert!(
        last.content
            .iter()
            .any(|p| matches!(p, ContentPart::Text { text } if text == INTERRUPTION_MARKER)),
        "cancellation must record the interruption marker"
    );

    // The turn is over, so the handle reports idle and refuses further steers.
    assert!(handle.active_turn_id().is_none());
}
