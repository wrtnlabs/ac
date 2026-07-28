//! Transient tool output is visible to the producing turn without becoming
//! durable session history. This is the generic seam used by image-view tools.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ac_provider_mock::{MockProvider, stop_end, stop_tool_use, text, tool_use};
use ac_runtime::{
    AgentConfig, AgentEvent, CompactionConfig, CompactionStrategy, Session, StepPrepareHook,
};
use ac_tool::{Capability, SubtreePolicy, Tool, ToolCtx, ToolOutput, ToolRegistry};
use ac_types::{CompletionEvent, ContentPart, Message, Role, TokenUsage};
use serde::Deserialize;

#[derive(Deserialize, schemars::JsonSchema)]
struct ImageInput {
    index: u8,
}

struct ImageTool;

impl Tool for ImageTool {
    type Input = ImageInput;

    fn name(&self) -> &'static str {
        "see"
    }

    fn description(&self) -> String {
        "returns live images with a durable placeholder".into()
    }

    fn capability(&self) -> Capability {
        Capability::ReadOnly
    }

    fn run(
        self: Arc<Self>,
        input: Self::Input,
        _ctx: Arc<ToolCtx>,
    ) -> futures::future::BoxFuture<'static, ToolOutput> {
        Box::pin(async move {
            match input.index {
                1 => ToolOutput::ok("live-envelope-1")
                    .with_durable_content("durable-placeholder-1")
                    .with_image("image/png", "QUJD")
                    .with_image("image/webp", "REVG"),
                2 => ToolOutput::ok("live-envelope-2")
                    .with_durable_content("durable-placeholder-2")
                    .with_image("image/jpeg", "R0hJ"),
                other => ToolOutput::error(format!("unexpected image {other}")),
            }
        })
    }
}

/// Deliberately edits one live result on only the second sampling request.
/// Seeing `live-envelope-1` proves transient projection ran first; retaining
/// the edit proves the hook remains final outgoing-request authority.
struct RedactSecondRequest {
    calls: AtomicUsize,
}

impl StepPrepareHook for RedactSecondRequest {
    fn prepare(&self, _iteration: usize, request: &mut ac_provider::CompletionRequest) {
        if self.calls.fetch_add(1, Ordering::SeqCst) != 1 {
            return;
        }
        for message in &mut request.messages {
            for part in &mut message.content {
                if let ContentPart::ToolResult(result) = part
                    && result.content == "live-envelope-1"
                {
                    result.content = "hook-redacted-live-1".into();
                }
            }
        }
    }
}

fn context() -> (Arc<ToolCtx>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let policy = SubtreePolicy::new(dir.path()).unwrap();
    (Arc::new(ToolCtx::new(Arc::new(policy))), dir)
}

fn image_parts(messages: &[Message]) -> Vec<(&str, &str)> {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|part| match part {
            ContentPart::Image { media_type, data } => Some((media_type.as_str(), data.as_str())),
            _ => None,
        })
        .collect()
}

fn result_contents(messages: &[Message]) -> Vec<&str> {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|part| match part {
            ContentPart::ToolResult(result) => Some(result.content.as_str()),
            _ => None,
        })
        .collect()
}

fn usage(input_tokens: u64) -> CompletionEvent {
    CompletionEvent::UsageUpdate(TokenUsage {
        input_tokens,
        ..TokenUsage::default()
    })
}

fn tool_use_ids(messages: &[Message]) -> Vec<&str> {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|part| match part {
            ContentPart::ToolUse(tool_use) => Some(tool_use.id.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn transient_parts_are_live_after_hooks_but_never_durable() {
    let provider = MockProvider::new(vec![
        vec![
            tool_use("call-1", "see", serde_json::json!({ "index": 1 })),
            tool_use("call-2", "see", serde_json::json!({ "index": 2 })),
            stop_tool_use(),
        ],
        vec![text("saw them"), stop_end()],
        // A second user turn on the same Session must not replay prior pixels.
        vec![text("later"), stop_end()],
    ]);
    let provider_handle = provider.clone();
    let (ctx, _dir) = context();
    let mut registry = ToolRegistry::new();
    registry.register(ImageTool);
    let registry = Arc::new(registry);
    let mut session = Session::new(
        Arc::new(provider),
        registry.clone(),
        ctx,
        AgentConfig::default(),
    );
    session.add_step_hook(Arc::new(RedactSecondRequest {
        calls: AtomicUsize::new(0),
    }));

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    session.run_turn("look".into(), tx).await.unwrap();
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }

    let live_results: Vec<(&str, &str)> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolResult { id, output, .. } => Some((id.as_str(), output.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(
        live_results,
        vec![("call-1", "live-envelope-1"), ("call-2", "live-envelope-2")],
        "observers receive the current content, not its durable fallback"
    );

    let requests = provider_handle.requests();
    let second = &requests[1].messages;
    assert_eq!(
        result_contents(second),
        vec!["hook-redacted-live-1", "live-envelope-2"],
        "the hook sees transient content and retains final request authority"
    );
    let result_row = second
        .iter()
        .position(|message| {
            message
                .content
                .iter()
                .any(|part| matches!(part, ContentPart::ToolResult(_)))
        })
        .unwrap();
    assert_eq!(
        second[result_row + 1].role,
        Role::User,
        "transient images directly follow the tool-result message"
    );
    assert_eq!(
        image_parts(&second[result_row + 1..=result_row + 1]),
        vec![
            ("image/png", "QUJD"),
            ("image/webp", "REVG"),
            ("image/jpeg", "R0hJ")
        ],
        "concurrent result order and each tool's part order are preserved"
    );

    let durable_history = session.messages();
    assert_eq!(
        result_contents(&durable_history),
        vec!["durable-placeholder-1", "durable-placeholder-2"]
    );
    assert!(image_parts(&durable_history).is_empty());
    let serialized = serde_json::to_string(&durable_history).unwrap();
    for pixels in ["QUJD", "REVG", "R0hJ"] {
        assert!(
            !serialized.contains(pixels),
            "base64 {pixels} leaked into Session::messages"
        );
    }
    assert!(!serialized.contains("live-envelope"));

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    session.run_turn("again".into(), tx).await.unwrap();
    let third = &provider_handle.requests()[2].messages;
    assert_eq!(
        result_contents(third),
        vec!["durable-placeholder-1", "durable-placeholder-2"]
    );
    assert!(
        image_parts(third).is_empty(),
        "a later turn must not retain transient parts"
    );

    // Flat-history recovery is the host persistence path. It has no runtime
    // side table to reconstruct, and must therefore sample only the fallback.
    let resumed_provider = MockProvider::new(vec![vec![text("resumed"), stop_end()]]);
    let resumed_handle = resumed_provider.clone();
    let (resumed_ctx, _resumed_dir) = context();
    let mut resumed = Session::resume(
        Arc::new(resumed_provider),
        registry,
        resumed_ctx,
        AgentConfig::default(),
        durable_history,
    );
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    resumed.run_turn("resume".into(), tx).await.unwrap();
    let resumed_request = &resumed_handle.requests()[0].messages;
    assert_eq!(
        result_contents(resumed_request),
        vec!["durable-placeholder-1", "durable-placeholder-2"]
    );
    assert!(image_parts(resumed_request).is_empty());
}

#[tokio::test]
async fn byte_budget_evicts_the_oldest_call_without_orphaning_its_images() {
    let provider = MockProvider::new(vec![
        vec![
            tool_use("call-1", "see", serde_json::json!({ "index": 1 })),
            tool_use("call-2", "see", serde_json::json!({ "index": 2 })),
            stop_tool_use(),
        ],
        vec![text("done"), stop_end()],
    ]);
    let handle = provider.clone();
    let (ctx, _dir) = context();
    let mut registry = ToolRegistry::new();
    registry.register(ImageTool);
    let config = AgentConfig {
        // call-1 fits alone and call-2 fits alone; together they do not.
        transient_tool_output_bytes: 50,
        ..AgentConfig::default()
    };
    let mut session = Session::new(Arc::new(provider), Arc::new(registry), ctx, config);

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    session.run_turn("look".into(), tx).await.unwrap();

    let second = &handle.requests()[1].messages;
    assert_eq!(
        result_contents(second),
        vec!["durable-placeholder-1", "live-envelope-2"],
        "the first call falls back durably while the newest stays live"
    );
    assert_eq!(
        image_parts(second),
        vec![("image/jpeg", "R0hJ")],
        "eviction must remove the old call's images as a unit"
    );
    assert!(
        second.iter().all(|message| !message.content.iter().any(
            |part| matches!(part, ContentPart::Image { data, .. } if data == "QUJD" || data == "REVG")
        )),
        "an evicted call must not leave orphan image messages"
    );
    assert_eq!(
        result_contents(&session.messages()),
        vec!["durable-placeholder-1", "durable-placeholder-2"]
    );
}

async fn assert_transient_survives_mid_turn_compaction(strategy: CompactionStrategy) {
    let mut scripted = vec![vec![
        tool_use("image-call", "see", serde_json::json!({ "index": 1 })),
        usage(50_000),
        stop_tool_use(),
    ]];
    if strategy == CompactionStrategy::Summarize {
        scripted.push(vec![text("durable handoff"), stop_end()]);
    }
    scripted.extend([
        // Force one more sampling request in the same turn after the
        // post-compaction request consumed the image.
        vec![
            tool_use("plain-call", "see", serde_json::json!({ "index": 9 })),
            stop_tool_use(),
        ],
        vec![text("done"), stop_end()],
        // A later turn on the same session must not replay the image.
        vec![text("later"), stop_end()],
    ]);

    let provider = MockProvider::new(scripted);
    let handle = provider.clone();
    let (ctx, _dir) = context();
    let mut registry = ToolRegistry::new();
    registry.register(ImageTool);
    let registry = Arc::new(registry);
    let config = AgentConfig {
        compaction: Some(CompactionConfig {
            budget_tokens: 1_000,
            per_message_cap_tokens: 4_096,
            summary_max_tokens: 2_048,
            exclude_cached_prefix: false,
            strategy,
            handoff_system: None,
        }),
        ..AgentConfig::default()
    };
    let mut session = Session::new(Arc::new(provider), registry.clone(), ctx, config);

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    session.run_turn("look".into(), tx).await.unwrap();

    let requests = handle.requests();
    let continuation_index = match strategy {
        CompactionStrategy::Summarize => {
            let summary_request = &requests[1].messages;
            assert_eq!(
                result_contents(summary_request),
                vec!["durable-placeholder-1"],
                "the compaction summary reads the durable fallback"
            );
            assert!(
                image_parts(summary_request).is_empty(),
                "the internal summary request must not consume or persist pixels"
            );
            2
        }
        CompactionStrategy::FreshWindow => 1,
    };

    let continuation = &requests[continuation_index].messages;
    assert_eq!(
        result_contents(continuation),
        vec!["live-envelope-1"],
        "the first ordinary sample after compaction receives live content"
    );
    assert_eq!(
        image_parts(continuation),
        vec![("image/png", "QUJD"), ("image/webp", "REVG")],
        "mid-turn compaction must not strand the transient pixels"
    );
    assert_eq!(
        tool_use_ids(continuation),
        vec!["image-call"],
        "the ephemeral result retains its matching provider call"
    );

    let later_same_turn = &requests[continuation_index + 1].messages;
    assert!(
        !tool_use_ids(later_same_turn).contains(&"image-call"),
        "the compacted tool call is offered for exactly one sample"
    );
    assert!(
        !result_contents(later_same_turn).contains(&"live-envelope-1"),
        "live content is consumed by the first post-compaction sample"
    );
    assert!(
        image_parts(later_same_turn).is_empty(),
        "pixels are not replayed on a later sample in the same turn"
    );

    // The append-only persistence log records only the durable result. The
    // ephemeral replay and its pixels never cross the session boundary.
    let jsonl = session.rollout().to_jsonl();
    assert!(jsonl.contains("durable-placeholder-1"));
    for private in ["live-envelope-1", "QUJD", "REVG"] {
        assert!(
            !jsonl.contains(private),
            "{private} leaked into the durable rollout"
        );
    }
    let persisted_rollout = session.rollout().clone();

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    session.run_turn("again".into(), tx).await.unwrap();
    let later_turn = &handle.requests()[continuation_index + 2].messages;
    assert!(!result_contents(later_turn).contains(&"live-envelope-1"));
    assert!(image_parts(later_turn).is_empty());

    let resumed_provider = MockProvider::new(vec![vec![text("resumed"), stop_end()]]);
    let resumed_handle = resumed_provider.clone();
    let (resumed_ctx, _resumed_dir) = context();
    let mut resumed = Session::resume_from(
        Arc::new(resumed_provider),
        registry,
        resumed_ctx,
        AgentConfig {
            compaction: Some(CompactionConfig {
                budget_tokens: 1_000,
                per_message_cap_tokens: 4_096,
                summary_max_tokens: 2_048,
                exclude_cached_prefix: false,
                strategy,
                handoff_system: None,
            }),
            ..AgentConfig::default()
        },
        persisted_rollout,
    );
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    resumed.run_turn("resume".into(), tx).await.unwrap();
    let resumed_request = &resumed_handle.requests()[0].messages;
    assert!(!result_contents(resumed_request).contains(&"live-envelope-1"));
    assert!(image_parts(resumed_request).is_empty());
}

#[tokio::test]
async fn transient_output_survives_summarizing_mid_turn_compaction_once() {
    assert_transient_survives_mid_turn_compaction(CompactionStrategy::Summarize).await;
}

#[tokio::test]
async fn transient_output_survives_fresh_window_mid_turn_compaction_once() {
    assert_transient_survives_mid_turn_compaction(CompactionStrategy::FreshWindow).await;
}
