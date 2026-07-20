//! A scripted [`Provider`] for deterministic, offline end-to-end tests.
//!
//! Construct one with an ordered list of *turns*. Each turn is the event
//! sequence the model would stream for one request. `stream_completion` pops
//! the next turn per call, so a two-turn script models "call a tool, then
//! answer": turn 0 emits a `ToolUse`, turn 1 emits `Text` + `Stop`.
//!
//! It also records the [`CompletionRequest`] it received on each call, so tests
//! can assert that tool results were fed back into the next request.

use std::sync::{Arc, Mutex};

use ac_provider::{CompletionRequest, EventStream, Provider};
use ac_types::{CompletionError, CompletionEvent, StopReason, ToolUse};
use futures::future::BoxFuture;

#[derive(Clone)]
pub struct MockProvider {
    turns: Arc<Mutex<std::collections::VecDeque<Vec<CompletionEvent>>>>,
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
}

impl MockProvider {
    pub fn new(turns: Vec<Vec<CompletionEvent>>) -> Self {
        Self {
            turns: Arc::new(Mutex::new(turns.into())),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Every request the provider was asked to stream, in order. Lets a test
    /// assert the loop fed tool results back.
    pub fn requests(&self) -> Vec<CompletionRequest> {
        self.requests.lock().unwrap().clone()
    }

    pub fn call_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

impl Provider for MockProvider {
    fn name(&self) -> &str {
        "mock"
    }

    fn stream_completion(
        &self,
        request: CompletionRequest,
    ) -> BoxFuture<'static, Result<EventStream, CompletionError>> {
        self.requests.lock().unwrap().push(request);
        let next = self.turns.lock().unwrap().pop_front();
        Box::pin(async move {
            // A script that runs dry ends the turn cleanly rather than hanging
            // the loop — an over-eager runtime bug then surfaces as an assertion
            // on call_count, not a deadlock.
            let events = next.unwrap_or_else(|| vec![CompletionEvent::Stop(StopReason::EndTurn)]);
            let stream = async_stream::try_stream! {
                for event in events {
                    yield event;
                }
            };
            Ok(Box::pin(stream) as EventStream)
        })
    }
}

/// Convenience builders for the two event kinds tests reach for most.
pub fn text(s: impl Into<String>) -> CompletionEvent {
    CompletionEvent::Text(s.into())
}

pub fn tool_use(
    id: impl Into<String>,
    name: impl Into<String>,
    input: serde_json::Value,
) -> CompletionEvent {
    CompletionEvent::ToolUse(ToolUse {
        id: id.into(),
        name: name.into(),
        input,
    })
}

pub fn stop_end() -> CompletionEvent {
    CompletionEvent::Stop(StopReason::EndTurn)
}

pub fn stop_tool_use() -> CompletionEvent {
    CompletionEvent::Stop(StopReason::ToolUse)
}
