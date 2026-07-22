//! The agent loop: a `Session` that drives a `Provider` and a `ToolRegistry`
//! until the model stops asking for tools, emitting a typed `AgentEvent` stream.

mod steer;

use std::sync::Arc;
use std::time::Duration;

use ac_provider::{CompletionRequest, Provider, ServerTool};
use ac_tool::{ToolCtx, ToolRegistry};
use ac_types::{
    CompletionEvent, ContentPart, Message, Role, StopReason, TokenUsage, ToolResult, ToolUse,
};
use futures::StreamExt;
use tokio::sync::mpsc::UnboundedSender;

pub use steer::{SteerError, SteerHandle, SteerInput, TurnClass};

use steer::SteerState;

/// Recorded into history when a turn is cancelled on purpose, so the next
/// turn's model reads the interruption as deliberate — not an anomaly to
/// re-attempt — and knows partial effects may have landed
/// ([docs/ac-queue-steer.md] §5, [docs/ac-fork.md] I6).
pub const INTERRUPTION_MARKER: &str = "The previous turn was interrupted on purpose. Any commands or tools it had started may \
     have partially executed; do not assume its work completed.";

/// Deactivates the active turn when the turn's scope ends, on every exit path
/// including a panic unwind — so a stale active turn never outlives its
/// `run_turn`.
struct ActiveTurnGuard {
    state: Arc<SteerState>,
    id: String,
}

impl Drop for ActiveTurnGuard {
    fn drop(&mut self) {
        self.state.deactivate(&self.id);
    }
}

/// Static configuration for a `Session`.
pub struct AgentConfig {
    pub model: String,
    pub system: Option<String>,
    pub max_iterations: usize,
    /// Provider-executed server tools to request every round-trip (e.g. web
    /// search). Provider-agnostic intent — a provider that can't do one ignores
    /// it. These are NOT local tools and never touch the registry.
    pub server_tools: Vec<ServerTool>,
    /// Max time to wait for the next stream event before giving up on a stalled
    /// provider. `None` disables the guard. Defaults to 5 minutes.
    pub idle_timeout: Option<Duration>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            system: None,
            max_iterations: 16,
            server_tools: Vec::new(),
            idle_timeout: Some(Duration::from_secs(300)),
        }
    }
}

/// A typed event emitted as the loop makes progress. Serializable so hosts
/// can put it on a wire (a daemon socket, a WebSocket) or in a log; the tag
/// layout is part of the kit's public surface — change it deliberately.
/// Adjacently tagged (`{"type": …, "data": …}`) — internal tagging cannot
/// represent newtype variants of primitives (`Text(String)` fails at runtime).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum AgentEvent {
    Text(String),
    Thinking(String),
    ToolCall {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        id: String,
        name: String,
        output: String,
        is_error: bool,
    },
    /// A source cited by a provider-executed server tool (e.g. web search).
    Citation {
        url: String,
        title: Option<String>,
    },
    Usage(TokenUsage),
    TurnComplete {
        stop_reason: StopReason,
    },
    Error(String),
}

/// Hook invoked before each model round-trip; may swap model, filter tools,
/// edit the system prompt, or set the tool choice.
pub trait StepHook: Send + Sync {
    fn prepare(&self, iteration: usize, request: &mut CompletionRequest);
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("completion failed: {0}")]
    Completion(#[from] ac_types::CompletionError),
    #[error("exceeded max iterations ({0})")]
    MaxIterations(usize),
    #[error("provider stalled: no event within the idle timeout")]
    Timeout,
    #[error("cancelled")]
    Cancelled,
}

/// A conversational session that owns the message history and drives the loop.
pub struct Session {
    provider: Arc<dyn Provider>,
    registry: Arc<ToolRegistry>,
    ctx: Arc<ToolCtx>,
    config: AgentConfig,
    hooks: Vec<Arc<dyn StepHook>>,
    messages: Vec<Message>,
    steer: Arc<SteerState>,
}

impl Session {
    pub fn new(
        provider: Arc<dyn Provider>,
        registry: Arc<ToolRegistry>,
        ctx: Arc<ToolCtx>,
        config: AgentConfig,
    ) -> Self {
        Self {
            provider,
            registry,
            ctx,
            config,
            hooks: Vec::new(),
            messages: Vec::new(),
            steer: Arc::new(SteerState::new()),
        }
    }

    /// Rebuild a session from persisted history — the reload-recovery path.
    /// The kit doesn't know where history lives (SQLite, JSONL, a test
    /// fixture); the host loads it and hands it back.
    pub fn resume(
        provider: Arc<dyn Provider>,
        registry: Arc<ToolRegistry>,
        ctx: Arc<ToolCtx>,
        config: AgentConfig,
        history: Vec<Message>,
    ) -> Self {
        let mut session = Self::new(provider, registry, ctx, config);
        session.messages = history;
        session
    }

    /// Install a step hook. Hooks compose: each runs in registration order on
    /// every model round-trip, each seeing the previous hooks' edits.
    pub fn add_hook(&mut self, hook: Arc<dyn StepHook>) {
        self.hooks.push(hook);
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// A handle for submitting mid-turn input to whatever turn is running on
    /// this session. Obtain it before starting the turn; use it from another
    /// task while `run_turn` executes ([docs/ac-queue-steer.md]).
    pub fn steer_handle(&self) -> SteerHandle {
        SteerHandle::new(self.steer.clone())
    }

    /// Move the active turn's pending steers into history as plain user
    /// messages, unsampled — the terminal-flush of [docs/ac-queue-steer.md] §4:
    /// input the runtime accepted reaches history even when the turn ends
    /// abnormally (R2), except under deliberate cancellation ([`on_user_cancel`]).
    fn flush_pending(&mut self) {
        for item in self.steer.take_pending() {
            match item {
                SteerInput::Text(t) => self.messages.push(Message::text(Role::User, t)),
            }
        }
    }

    /// Deliberate cancellation ([docs/ac-queue-steer.md] §5): discard the
    /// pending queue (the user said stop, including what they just typed) and
    /// record the interruption marker so the next turn's model reads the cut as
    /// intentional.
    fn on_user_cancel(&mut self) {
        let _ = self.steer.take_pending();
        self.messages
            .push(Message::text(Role::User, INTERRUPTION_MARKER));
    }

    pub async fn run_turn(
        &mut self,
        user_text: String,
        sink: UnboundedSender<AgentEvent>,
    ) -> Result<StopReason, RuntimeError> {
        // Cloned so the turn's `tokio::select!` futures don't borrow `self`,
        // leaving `&mut self` free to record markers in the branch bodies.
        let cancel = self.ctx.cancel.clone();
        let provider = self.provider.clone();

        self.messages.push(Message::text(Role::User, user_text));

        // The turn's own input (I₀) must sample before any steer, so draining
        // is deferred (`drainable = false`) until the first step completes; the
        // guard deactivates the turn on every exit, including a panic.
        let turn_id = self.steer.activate(TurnClass::Regular);
        let _turn_guard = ActiveTurnGuard {
            state: self.steer.clone(),
            id: turn_id.clone(),
        };
        let mut drainable = false;

        let mut iteration = 0usize;
        loop {
            if iteration >= self.config.max_iterations {
                self.flush_pending();
                return Err(RuntimeError::MaxIterations(self.config.max_iterations));
            }
            if cancel.is_cancelled() {
                self.on_user_cancel();
                return Err(RuntimeError::Cancelled);
            }
            // A dropped receiver means nobody is listening — treat it as an
            // implicit cancel so we stop spending tokens and running tools.
            // Not deliberate user intent: discard the queue, but record no
            // interruption marker (the client simply went away).
            if sink.is_closed() {
                let _ = self.steer.take_pending();
                return Err(RuntimeError::Cancelled);
            }

            // Step boundary: drain pending steers into history as plain user
            // messages ([docs/ac-queue-steer.md] §4). `drainable` gates the
            // initial deferral above.
            if drainable {
                for item in self.steer.take_pending() {
                    match item {
                        SteerInput::Text(t) => self.messages.push(Message::text(Role::User, t)),
                    }
                }
            }

            let mut req = CompletionRequest::new(&self.config.model);
            req.system = self.config.system.clone();
            req.cache_system = self.config.system.is_some();
            req.messages = self.messages.clone();
            req.tools = self.registry.specs();
            req.server_tools = self.config.server_tools.clone();

            for hook in &self.hooks {
                hook.prepare(iteration, &mut req);
            }

            // Await the connection, but let a cancel break out of it.
            let mut stream = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    self.on_user_cancel();
                    return Err(RuntimeError::Cancelled);
                }
                res = provider.stream_completion(req) => match res {
                    Ok(s) => s,
                    Err(e) => {
                        self.flush_pending();
                        return Err(RuntimeError::Completion(e));
                    }
                },
            };

            let mut text = String::new();
            let mut tool_uses: Vec<ToolUse> = Vec::new();
            let mut stop_reason = StopReason::EndTurn;

            loop {
                // Race the next event against cancellation and an idle timeout so
                // a stalled or never-closing stream can't wedge the turn.
                let next = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        self.on_user_cancel();
                        return Err(RuntimeError::Cancelled);
                    }
                    n = async {
                        match self.config.idle_timeout {
                            Some(d) => tokio::time::timeout(d, stream.next()).await.map_err(|_| ()),
                            None => Ok(stream.next().await),
                        }
                    } => n,
                };
                let event = match next {
                    Err(()) => {
                        self.flush_pending();
                        return Err(RuntimeError::Timeout);
                    }
                    Ok(None) => break,
                    Ok(Some(Ok(ev))) => ev,
                    Ok(Some(Err(e))) => {
                        self.flush_pending();
                        return Err(RuntimeError::Completion(e));
                    }
                };
                match event {
                    CompletionEvent::Text(s) => {
                        text.push_str(&s);
                        let _ = sink.send(AgentEvent::Text(s));
                    }
                    CompletionEvent::Thinking { text: t, .. } => {
                        let _ = sink.send(AgentEvent::Thinking(t));
                    }
                    CompletionEvent::ToolUse(tu) => {
                        tool_uses.push(tu);
                    }
                    CompletionEvent::Citation(c) => {
                        let _ = sink.send(AgentEvent::Citation {
                            url: c.url,
                            title: c.title,
                        });
                    }
                    CompletionEvent::UsageUpdate(u) => {
                        let _ = sink.send(AgentEvent::Usage(u));
                    }
                    CompletionEvent::Stop(reason) => {
                        stop_reason = reason;
                        break;
                    }
                }
            }

            let mut assistant_content: Vec<ContentPart> = Vec::new();
            if !text.is_empty() {
                assistant_content.push(ContentPart::Text { text });
            }
            for tu in &tool_uses {
                assistant_content.push(ContentPart::ToolUse(tu.clone()));
            }
            self.messages.push(Message {
                role: Role::Assistant,
                content: assistant_content,
                cache: false,
            });

            // A completed step makes the queue drainable from here on.
            drainable = true;

            // No tool calls: the model owes no continuation, so the turn ends —
            // unless a steer is pending, which extends it for one more step
            // ([docs/ac-queue-steer.md] §4). `end_if_idle` makes the empty-check
            // and deactivation atomic, closing the terminal race.
            if tool_uses.is_empty() {
                if self.steer.end_if_idle(&turn_id) {
                    let _ = sink.send(AgentEvent::TurnComplete { stop_reason });
                    return Ok(stop_reason);
                }
                iteration += 1;
                continue;
            }

            // Spawn each tool on its own task: they run concurrently, and a
            // panic in one becomes a JoinError we turn into an error result
            // rather than unwinding the turn. That guarantees every tool_use
            // gets exactly one tool_result — the invariant that keeps the
            // message history valid for the next request.
            let mut handles = Vec::with_capacity(tool_uses.len());
            for tu in &tool_uses {
                let _ = sink.send(AgentEvent::ToolCall {
                    id: tu.id.clone(),
                    name: tu.name.clone(),
                    input: tu.input.clone(),
                });
                let registry = self.registry.clone();
                let ctx = self.ctx.clone();
                let name = tu.name.clone();
                let input = tu.input.clone();
                let handle = tokio::spawn(async move { registry.run(&name, input, ctx).await });
                handles.push((tu.id.clone(), tu.name.clone(), handle));
            }

            let mut user_content: Vec<ContentPart> = Vec::with_capacity(handles.len());
            for (id, name, handle) in handles {
                let (content, is_error) = match handle.await {
                    Ok(out) => (out.content, out.is_error),
                    Err(e) => (format!("tool '{name}' panicked: {e}"), true),
                };
                let _ = sink.send(AgentEvent::ToolResult {
                    id: id.clone(),
                    name,
                    output: content.clone(),
                    is_error,
                });
                user_content.push(ContentPart::ToolResult(ToolResult {
                    tool_use_id: id,
                    content,
                    is_error,
                }));
            }
            self.messages.push(Message {
                role: Role::User,
                content: user_content,
                cache: false,
            });

            iteration += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tag layout is public surface: every variant must round-trip.
    #[test]
    fn every_agent_event_variant_round_trips() {
        let events = vec![
            AgentEvent::Text("hi".into()),
            AgentEvent::Thinking("hm".into()),
            AgentEvent::ToolCall {
                id: "c1".into(),
                name: "read_file".into(),
                input: serde_json::json!({ "path": "a.txt" }),
            },
            AgentEvent::ToolResult {
                id: "c1".into(),
                name: "read_file".into(),
                output: "ok".into(),
                is_error: false,
            },
            AgentEvent::Citation {
                url: "https://example.com".into(),
                title: Some("Example".into()),
            },
            AgentEvent::Usage(TokenUsage::default()),
            AgentEvent::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
            AgentEvent::Error("boom".into()),
        ];
        for event in events {
            let json = serde_json::to_string(&event)
                .unwrap_or_else(|e| panic!("serialize {event:?}: {e}"));
            let back: AgentEvent =
                serde_json::from_str(&json).unwrap_or_else(|e| panic!("deserialize {json}: {e}"));
            assert_eq!(
                std::mem::discriminant(&event),
                std::mem::discriminant(&back)
            );
        }
    }
}
