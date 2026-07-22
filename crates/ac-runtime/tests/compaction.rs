//! Integration proof of the compaction lifecycle ([docs/ac-compaction.md])
//! through the real run loop: the pre-turn, mid-turn, and manual triggers; the
//! summarize and fresh-window strategies; user-input fidelity (R2); the terminal
//! placement of `σ` (I4); and the R3 effectiveness guard. The provider is the
//! scripted mock, so every assertion is deterministic and offline.

use std::sync::Arc;

use ac_provider_mock::{MockProvider, stop_end, stop_tool_use, text, tool_use};
use ac_runtime::{
    AgentConfig, AgentEvent, CompactionConfig, CompactionError, CompactionStrategy,
    CompactionTrigger, Session,
};
use ac_tool::{Capability, SubtreePolicy, Tool, ToolCtx, ToolOutput, ToolRegistry};
use ac_types::{CompletionEvent, ContentPart, Message, Role, StopReason, TokenUsage};
use serde::Deserialize;
use tokio::sync::mpsc;

fn make_ctx() -> (Arc<ToolCtx>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let policy = SubtreePolicy::new(dir.path()).unwrap();
    (Arc::new(ToolCtx::new(Arc::new(policy))), dir)
}

/// Usage event that pushes measured `τ` to `input` tokens — the lever a test
/// pulls to cross the compaction budget deterministically.
fn usage(input: u64) -> CompletionEvent {
    CompletionEvent::UsageUpdate(TokenUsage {
        input_tokens: input,
        ..TokenUsage::default()
    })
}

/// A tool whose output is large — the "agent traffic" that compaction compresses
/// into `σ` and that must be gone from the post-compaction view.
#[derive(Clone)]
struct BigTool {
    marker: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct Empty {}

impl Tool for BigTool {
    type Input = Empty;
    fn name(&self) -> &'static str {
        "bigtool"
    }
    fn description(&self) -> String {
        "returns a large output".into()
    }
    fn capability(&self) -> Capability {
        Capability::ReadOnly
    }
    fn run(
        self: Arc<Self>,
        _input: Self::Input,
        _ctx: Arc<ToolCtx>,
    ) -> futures::future::BoxFuture<'static, ToolOutput> {
        let out = format!("{}{}", self.marker, "X".repeat(40_000));
        Box::pin(async move { ToolOutput::ok(out) })
    }
}

async fn run(
    mut session: Session,
    prompt: &str,
) -> (
    Result<StopReason, ac_runtime::RuntimeError>,
    Vec<AgentEvent>,
) {
    let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();
    let prompt = prompt.to_string();
    let driver = tokio::spawn(async move { session.run_turn(prompt, tx).await });
    let mut events = Vec::new();
    while let Some(ev) = rx.recv().await {
        events.push(ev);
    }
    (driver.await.expect("join"), events)
}

/// All the human-readable text a message carries — both genuine text parts and
/// tool-result bodies (the agent traffic), so an assertion can see whether the
/// big tool output survived a compaction.
fn text_of(m: &Message) -> String {
    m.content
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text { text } => Some(text.clone()),
            ContentPart::ToolResult(r) => Some(r.content.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn joined(msgs: &[Message]) -> String {
    msgs.iter().map(text_of).collect::<Vec<_>>().join("\n")
}

fn compacted_event(events: &[AgentEvent]) -> Option<(&str, &str)> {
    events.iter().find_map(|e| match e {
        AgentEvent::Compacted {
            trigger, summary, ..
        } => Some((trigger.as_str(), summary.as_str())),
        _ => None,
    })
}

fn summarize_config(budget: u64) -> CompactionConfig {
    CompactionConfig {
        budget_tokens: budget,
        per_message_cap_tokens: 4_096,
        summary_max_tokens: 2_048,
        exclude_cached_prefix: false,
        strategy: CompactionStrategy::Summarize,
        handoff_system: None,
    }
}

#[tokio::test]
async fn mid_turn_compaction_checkpoints_and_continues_the_same_turn() {
    // Step 0 calls the big tool and reports huge usage; the mid-turn trigger
    // fires (τ ≥ β), a summary is produced, and step 1 runs against H′ — all in
    // one turn.
    let provider = MockProvider::new(vec![
        vec![
            tool_use("c1", "bigtool", serde_json::json!({})),
            usage(50_000),
            stop_tool_use(),
        ],
        vec![text("HANDOFF-SUMMARY-7788"), stop_end()],
        vec![text("final-answer"), stop_end()],
    ]);
    let handle = provider.clone();

    let mut registry = ToolRegistry::new();
    registry.register(BigTool {
        marker: "BIG-OUTPUT-4141".into(),
    });
    let (ctx, _dir) = make_ctx();
    let config = AgentConfig {
        compaction: Some(summarize_config(1_000)),
        ..Default::default()
    };
    let session = Session::new(Arc::new(provider), Arc::new(registry), ctx, config);

    let (result, events) = run(session, "begin").await;
    assert_eq!(result.expect("turn ok"), StopReason::EndTurn);

    let requests = handle.requests();
    assert_eq!(requests.len(), 3, "step 0, the summary call, step 1");

    // The summary call (request 1) is a no-tool handoff over the full transcript.
    assert!(
        requests[1].tools.is_empty(),
        "no tools during summarization"
    );
    assert!(
        requests[1].system.is_some(),
        "the handoff instruction is the system prompt"
    );
    assert!(
        joined(&requests[1].messages).contains("BIG-OUTPUT-4141"),
        "the summarizer sees the agent traffic it must compress"
    );

    // Step 1 (request 2) runs against H′: the summary is present, verbatim user
    // input survives, and the giant tool output is gone.
    let view = joined(&requests[2].messages);
    assert!(
        view.contains("HANDOFF-SUMMARY-7788"),
        "σ is in the post-compaction view"
    );
    assert!(
        view.contains("begin"),
        "the user's input survived verbatim (R2)"
    );
    assert!(
        !view.contains("BIG-OUTPUT-4141"),
        "the agent traffic is compressed away"
    );

    // The observer saw the record, tagged mid_turn (R4).
    assert_eq!(
        compacted_event(&events),
        Some(("mid_turn", "HANDOFF-SUMMARY-7788"))
    );

    // σ is the terminal item of the replacement (I4): the last message the final
    // step built on is the handoff fragment — it carries the summary and closes
    // with the handoff marker.
    let last = requests[2].messages.last().expect("a message");
    assert!(
        text_of(last).contains("HANDOFF-SUMMARY-7788"),
        "the handoff σ is the terminal item"
    );
    assert!(
        text_of(last).trim_end().ends_with("[End of handoff.]"),
        "the handoff fragment closes with its marker"
    );
}

#[tokio::test]
async fn repeated_compaction_does_not_accumulate_prior_summaries() {
    // Two mid-turn compactions in one turn. The second must fold the first's σ
    // into its own, NOT carry it forward verbatim — otherwise every past summary
    // piles up and the long task eventually dies on R3 (I5).
    let provider = MockProvider::new(vec![
        // step 0 → tool + over budget
        vec![
            tool_use("c1", "bigtool", serde_json::json!({})),
            usage(50_000),
            stop_tool_use(),
        ],
        vec![text("SUMMARY-ONE"), stop_end()], // compaction 1
        // step 1 (against H′₁) → tool + over budget again
        vec![
            tool_use("c2", "bigtool", serde_json::json!({})),
            usage(50_000),
            stop_tool_use(),
        ],
        vec![text("SUMMARY-TWO"), stop_end()], // compaction 2
        vec![text("final"), stop_end()],       // step 2 (against H′₂)
    ]);
    let handle = provider.clone();
    let mut registry = ToolRegistry::new();
    registry.register(BigTool {
        marker: "BIG-OUTPUT".into(),
    });
    let (ctx, _dir) = make_ctx();
    let config = AgentConfig {
        compaction: Some(summarize_config(1_000)),
        ..Default::default()
    };
    let session = Session::new(Arc::new(provider), Arc::new(registry), ctx, config);

    let (result, _events) = run(session, "begin").await;
    assert_eq!(result.expect("turn ok"), StopReason::EndTurn);

    let requests = handle.requests();
    // The final step (last request) runs against H′₂.
    let view = joined(&requests.last().unwrap().messages);
    assert!(view.contains("SUMMARY-TWO"), "the latest σ is present");
    assert!(
        view.contains("begin"),
        "the user input survived both compactions"
    );
    assert!(
        !view.contains("SUMMARY-ONE"),
        "the prior σ must NOT accumulate — it folds into the new σ"
    );
    assert!(!view.contains("BIG-OUTPUT"), "agent traffic compressed");
}

#[tokio::test]
async fn pre_turn_compaction_clears_the_runway_before_the_first_step() {
    // Turn one leaves τ over budget; turn two compacts before its first step,
    // preserving both turns' user inputs and summarizing the assistant traffic.
    let provider = MockProvider::new(vec![
        vec![text("assistant-reply-one"), usage(50_000), stop_end()],
        vec![text("PRE-HANDOFF-33"), stop_end()],
        vec![text("assistant-reply-two"), stop_end()],
    ]);
    let handle = provider.clone();
    let (ctx, _dir) = make_ctx();
    let config = AgentConfig {
        compaction: Some(summarize_config(1_000)),
        ..Default::default()
    };
    let mut session = Session::new(
        Arc::new(provider),
        Arc::new(ToolRegistry::new()),
        ctx,
        config,
    );

    // Turn one — drives τ up.
    let (tx1, mut rx1) = mpsc::unbounded_channel();
    let stop1 = session.run_turn("question-one".into(), tx1).await.unwrap();
    assert_eq!(stop1, StopReason::EndTurn);
    while rx1.try_recv().is_ok() {}

    // Turn two — pre-turn compaction fires.
    let (tx2, mut rx2) = mpsc::unbounded_channel();
    let stop2 = session.run_turn("question-two".into(), tx2).await.unwrap();
    assert_eq!(stop2, StopReason::EndTurn);
    let mut events = Vec::new();
    while let Ok(ev) = rx2.try_recv() {
        events.push(ev);
    }

    let requests = handle.requests();
    assert_eq!(
        requests.len(),
        3,
        "turn-one step, the summary call, turn-two step"
    );

    // Turn two's step (request 2) runs against H′.
    let view = joined(&requests[2].messages);
    assert!(view.contains("PRE-HANDOFF-33"), "σ is present");
    assert!(
        view.contains("question-one") && view.contains("question-two"),
        "all user input verbatim (R2)"
    );
    assert!(
        !view.contains("assistant-reply-one"),
        "the assistant traffic was summarized away"
    );

    assert_eq!(
        compacted_event(&events),
        Some(("pre_turn", "PRE-HANDOFF-33"))
    );
}

#[tokio::test]
async fn manual_compaction_is_budget_independent_and_records_the_handoff() {
    // Budget is far above τ, so no automatic trigger would ever fire — manual
    // compaction runs anyway.
    let provider = MockProvider::new(vec![
        vec![text("assistant-reply"), usage(10), stop_end()],
        vec![text("MANUAL-HANDOFF-99"), stop_end()],
    ]);
    let handle = provider.clone();
    let (ctx, _dir) = make_ctx();
    let config = AgentConfig {
        compaction: Some(summarize_config(1_000_000)),
        ..Default::default()
    };
    let mut session = Session::new(
        Arc::new(provider),
        Arc::new(ToolRegistry::new()),
        ctx,
        config,
    );

    let (tx, mut rx) = mpsc::unbounded_channel();
    session
        .run_turn("tell me something".into(), tx)
        .await
        .unwrap();
    while rx.try_recv().is_ok() {}

    let (evt_tx, mut evt_rx) = mpsc::unbounded_channel();
    let outcome = session.compact(&evt_tx).await.expect("manual compaction");
    assert_eq!(outcome.trigger, CompactionTrigger::Manual);
    assert!(
        outcome.summary_chars > 0,
        "summarize produced a non-empty handoff"
    );

    let mut events = Vec::new();
    while let Ok(ev) = evt_rx.try_recv() {
        events.push(ev);
    }
    assert_eq!(
        compacted_event(&events),
        Some(("manual", "MANUAL-HANDOFF-99"))
    );

    // The projected view is now U ⧺ ⟨σ⟩: the user input, then the handoff.
    let view = session.messages();
    assert!(joined(&view).contains("MANUAL-HANDOFF-99"));
    assert!(
        joined(&view).contains("tell me something"),
        "user input verbatim"
    );
    assert!(
        !joined(&view).contains("assistant-reply"),
        "agent traffic compressed"
    );
    assert_eq!(
        handle.call_count(),
        2,
        "the run turn plus the one summary call"
    );
}

#[tokio::test]
async fn fresh_window_strategy_resets_without_a_summary_call() {
    // Fresh window: σ = ∅, so H′ = U alone and there is NO summary round-trip —
    // the same record shape as summarize, differing only in σ (R4).
    let provider = MockProvider::new(vec![
        vec![
            tool_use("c1", "bigtool", serde_json::json!({})),
            usage(50_000),
            stop_tool_use(),
        ],
        vec![text("done"), stop_end()],
    ]);
    let handle = provider.clone();
    let mut registry = ToolRegistry::new();
    registry.register(BigTool {
        marker: "BIG-OUTPUT".into(),
    });
    let (ctx, _dir) = make_ctx();
    let mut cfg = summarize_config(1_000);
    cfg.strategy = CompactionStrategy::FreshWindow;
    let config = AgentConfig {
        compaction: Some(cfg),
        ..Default::default()
    };
    let session = Session::new(Arc::new(provider), Arc::new(registry), ctx, config);

    let (result, events) = run(session, "begin").await;
    assert_eq!(result.expect("turn ok"), StopReason::EndTurn);

    let requests = handle.requests();
    assert_eq!(
        requests.len(),
        2,
        "no summary call — step 0 then step 1 over H′"
    );

    let view = joined(&requests[1].messages);
    assert!(view.contains("begin"), "user input survives");
    assert!(!view.contains("BIG-OUTPUT"), "agent traffic dropped");

    // The record is still emitted, with an empty σ.
    assert_eq!(compacted_event(&events), Some(("mid_turn", "")));
}

#[tokio::test]
async fn nothing_to_compact_when_the_view_is_only_user_input() {
    // A history with no agent traffic cannot be compressed — manual compaction
    // reports it rather than producing a no-op record.
    let provider = MockProvider::new(vec![]);
    let (ctx, _dir) = make_ctx();
    let config = AgentConfig {
        compaction: Some(summarize_config(1_000)),
        ..Default::default()
    };
    let mut session = Session::resume(
        Arc::new(provider),
        Arc::new(ToolRegistry::new()),
        ctx,
        config,
        vec![Message::text(Role::User, "only user input here")],
    );

    let (tx, _rx) = mpsc::unbounded_channel();
    let err = session.compact(&tx).await.unwrap_err();
    assert!(matches!(err, CompactionError::NothingToCompact));
}

#[tokio::test]
async fn ineffective_compaction_is_an_error_not_a_loop() {
    // Budget so tight that even H′ exceeds it (R3): compaction surfaces an error
    // instead of clearing to a state that would immediately re-trigger.
    let provider = MockProvider::new(vec![vec![text("tiny"), stop_end()]]);
    let (ctx, _dir) = make_ctx();
    let mut cfg = summarize_config(1); // 1 token — unclearable
    cfg.per_message_cap_tokens = 100_000; // don't cap the user message away
    let config = AgentConfig {
        compaction: Some(cfg),
        ..Default::default()
    };
    let mut session = Session::resume(
        Arc::new(provider),
        Arc::new(ToolRegistry::new()),
        ctx,
        config,
        vec![
            Message::text(
                Role::User,
                "a reasonably long user message that will survive verbatim",
            ),
            Message::text(Role::Assistant, "assistant traffic to compress"),
        ],
    );

    let (tx, _rx) = mpsc::unbounded_channel();
    let err = session.compact(&tx).await.unwrap_err();
    match err {
        CompactionError::Ineffective { budget, achieved } => {
            assert_eq!(budget, 1);
            assert!(achieved >= 1);
        }
        other => panic!("expected Ineffective, got {other:?}"),
    }
}
