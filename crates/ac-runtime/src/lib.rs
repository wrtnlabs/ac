//! The agent loop: a `Session` that drives a `Provider` and a `ToolRegistry`
//! until the model stops asking for tools, emitting a typed `AgentEvent` stream.

use std::sync::Arc;
use std::time::Duration;

use ac_provider::{CompletionRequest, Provider};
use ac_tool::{ToolCtx, ToolRegistry};
use ac_types::{
    CompletionEvent, ContentPart, Message, Role, StopReason, TokenUsage, ToolResult, ToolUse,
};
use futures::StreamExt;
use tokio::sync::mpsc::UnboundedSender;

/// Static configuration for a `Session`.
pub struct AgentConfig {
    pub model: String,
    pub system: Option<String>,
    pub max_iterations: usize,
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
            idle_timeout: Some(Duration::from_secs(300)),
        }
    }
}

/// A typed event emitted as the loop makes progress.
#[derive(Debug, Clone)]
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
    hook: Option<Arc<dyn StepHook>>,
    messages: Vec<Message>,
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
            hook: None,
            messages: Vec::new(),
        }
    }

    pub fn set_hook(&mut self, hook: Arc<dyn StepHook>) {
        self.hook = Some(hook);
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub async fn run_turn(
        &mut self,
        user_text: String,
        sink: UnboundedSender<AgentEvent>,
    ) -> Result<StopReason, RuntimeError> {
        self.messages.push(Message::text(Role::User, user_text));

        let mut iteration = 0usize;
        loop {
            if iteration >= self.config.max_iterations {
                return Err(RuntimeError::MaxIterations(self.config.max_iterations));
            }
            if self.ctx.cancel.is_cancelled() {
                return Err(RuntimeError::Cancelled);
            }
            // A dropped receiver means nobody is listening — treat it as an
            // implicit cancel so we stop spending tokens and running tools.
            if sink.is_closed() {
                return Err(RuntimeError::Cancelled);
            }

            let mut req = CompletionRequest::new(&self.config.model);
            req.system = self.config.system.clone();
            req.cache_system = self.config.system.is_some();
            req.messages = self.messages.clone();
            req.tools = self.registry.specs();

            if let Some(hook) = &self.hook {
                hook.prepare(iteration, &mut req);
            }

            // Await the connection, but let a cancel break out of it.
            let mut stream = tokio::select! {
                biased;
                _ = self.ctx.cancel.cancelled() => return Err(RuntimeError::Cancelled),
                res = self.provider.stream_completion(req) => res?,
            };

            let mut text = String::new();
            let mut tool_uses: Vec<ToolUse> = Vec::new();
            let mut stop_reason = StopReason::EndTurn;

            loop {
                // Race the next event against cancellation and an idle timeout so
                // a stalled or never-closing stream can't wedge the turn.
                let next = tokio::select! {
                    biased;
                    _ = self.ctx.cancel.cancelled() => return Err(RuntimeError::Cancelled),
                    n = async {
                        match self.config.idle_timeout {
                            Some(d) => tokio::time::timeout(d, stream.next()).await.map_err(|_| ()),
                            None => Ok(stream.next().await),
                        }
                    } => n,
                };
                let event = match next {
                    Err(()) => return Err(RuntimeError::Timeout),
                    Ok(None) => break,
                    Ok(Some(Ok(ev))) => ev,
                    Ok(Some(Err(e))) => return Err(RuntimeError::Completion(e)),
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

            if tool_uses.is_empty() {
                let _ = sink.send(AgentEvent::TurnComplete { stop_reason });
                return Ok(stop_reason);
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
