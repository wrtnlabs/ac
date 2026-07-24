//! `collect_completion_text` — the public one-shot helper for short utility
//! completions. It must concatenate exactly the text events and honor the
//! cancel token, since `run_summary` (compaction's σ turn) rides it.

use ac_provider::CompletionRequest;
use ac_provider_mock::{MockProvider, stop_end, text};
use ac_runtime::{CollectTextError, collect_completion_text};
use ac_types::{CompletionEvent, TokenUsage};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn concatenates_text_events_into_one_string() {
    let provider = MockProvider::new(vec![vec![text("Hello, "), text("world"), stop_end()]]);
    let out = collect_completion_text(&provider, CompletionRequest::new("m"), None, None)
        .await
        .unwrap();
    assert_eq!(out, "Hello, world");
    assert_eq!(provider.call_count(), 1, "one shot means one request");
}

#[tokio::test]
async fn non_text_events_are_ignored() {
    let provider = MockProvider::new(vec![vec![
        CompletionEvent::Thinking {
            text: "pondering".into(),
            signature: None,
        },
        text("answer"),
        CompletionEvent::UsageUpdate(TokenUsage::default()),
        stop_end(),
    ]]);
    let out = collect_completion_text(&provider, CompletionRequest::new("m"), None, None)
        .await
        .unwrap();
    assert_eq!(out, "answer", "thinking and usage never leak into the text");
}

#[tokio::test]
async fn a_pre_cancelled_token_short_circuits() {
    let provider = MockProvider::new(vec![vec![text("never"), stop_end()]]);
    let cancel = CancellationToken::new();
    cancel.cancel();
    let err = collect_completion_text(&provider, CompletionRequest::new("m"), Some(&cancel), None)
        .await
        .expect_err("a cancelled token must not produce text");
    assert!(matches!(err, CollectTextError::Cancelled));
}
