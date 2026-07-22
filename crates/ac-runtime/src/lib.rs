//! The agent loop: a `Session` that drives a `Provider` and a `ToolRegistry`
//! until the model stops asking for tools, emitting a typed `AgentEvent` stream.
//!
//! The session is **log-backed**: its source of truth is an append-only
//! [`Rollout`] ([docs/ac-fork.md]), and "what the model sees" is the projection
//! `E(L)` of that log. Compaction ([docs/ac-compaction.md]) is therefore an
//! event in the log, not a mutation of a message buffer — which is what lets a
//! fork reproduce a pre- or post-compaction view for free.

mod compaction;
mod fragments;
mod steer;

use std::sync::Arc;
use std::time::Duration;

use ac_context::FragmentRegistry;
use ac_provider::{CompletionRequest, Provider, ServerTool};
use ac_rollout::Rollout;
use ac_tool::{ToolCtx, ToolRegistry};
use ac_types::{
    CompletionEvent, ContentPart, Message, Role, StopReason, TokenUsage, ToolResult, ToolUse,
};
use futures::StreamExt;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

pub use compaction::{
    CompactionConfig, CompactionError, CompactionOutcome, CompactionStrategy, CompactionTrigger,
};
pub use steer::{SteerError, SteerHandle, SteerInput, TurnClass};

use steer::SteerState;

pub use ac_types::INTERRUPTION_MARKER;

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
    /// Context-compaction budget and policy ([docs/ac-compaction.md]). `None`
    /// disables compaction: no trigger fires and manual `compact` is refused.
    pub compaction: Option<CompactionConfig>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            system: None,
            max_iterations: 16,
            server_tools: Vec::new(),
            idle_timeout: Some(Duration::from_secs(300)),
            compaction: None,
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
    /// The context was compacted ([docs/ac-compaction.md]). Observers receive
    /// the record itself, so what the context became is never ambiguous (R4);
    /// `trigger` is the one field that distinguishes compactions.
    Compacted {
        trigger: String,
        summary: String,
        tokens_before: u64,
        tokens_after: u64,
    },
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
    #[error("compaction failed: {0}")]
    Compaction(#[source] CompactionError),
}

/// A conversational session. Its history is an append-only [`Rollout`]; the
/// message list the model sees is `rollout.project()`.
pub struct Session {
    provider: Arc<dyn Provider>,
    registry: Arc<ToolRegistry>,
    ctx: Arc<ToolCtx>,
    config: AgentConfig,
    hooks: Vec<Arc<dyn StepHook>>,
    rollout: Rollout,
    /// The most recent server-reported usage — the source of truth for `τ`.
    last_usage: TokenUsage,
    /// Session-monotonic turn numbering (fork cut points).
    turn_counter: u64,
    /// Recognizes the runtime's own machine-injected fragments ([docs/ac-context.md]),
    /// so they are filtered from user input rather than promoted to instructions.
    fragments: FragmentRegistry,
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
            rollout: Rollout::create(),
            last_usage: TokenUsage::default(),
            turn_counter: 0,
            fragments: fragments::runtime_registry(),
            steer: Arc::new(SteerState::new()),
        }
    }

    /// Rebuild a session from a flat message history — the reload-recovery path
    /// for hosts that persist the projected view (e.g. a SQLite message table).
    /// Turn structure is not recoverable from a flat list; the history becomes a
    /// baseline the next turn builds on. Hosts that persist the full log resume
    /// via [`resume_from`](Self::resume_from) instead.
    pub fn resume(
        provider: Arc<dyn Provider>,
        registry: Arc<ToolRegistry>,
        ctx: Arc<ToolCtx>,
        config: AgentConfig,
        history: Vec<Message>,
    ) -> Self {
        let mut rollout = Rollout::create();
        for m in history {
            rollout.record_message(m);
        }
        Self::from_rollout(provider, registry, ctx, config, rollout)
    }

    /// Rebuild a session from a persisted [`Rollout`] — the full-fidelity resume
    /// (turn boundaries, compaction records, and lineage all intact).
    pub fn resume_from(
        provider: Arc<dyn Provider>,
        registry: Arc<ToolRegistry>,
        ctx: Arc<ToolCtx>,
        config: AgentConfig,
        rollout: Rollout,
    ) -> Self {
        Self::from_rollout(provider, registry, ctx, config, rollout)
    }

    fn from_rollout(
        provider: Arc<dyn Provider>,
        registry: Arc<ToolRegistry>,
        ctx: Arc<ToolCtx>,
        config: AgentConfig,
        rollout: Rollout,
    ) -> Self {
        let mut session = Self::new(provider, registry, ctx, config);
        // Continue numbering past the highest turn already in the log.
        let highest = rollout
            .cut_turns()
            .into_iter()
            .max()
            .unwrap_or(0)
            .max(rollout.open_turn().unwrap_or(0));
        session.turn_counter = highest;
        // Seed `τ` from a size estimate so a resumed session over budget can
        // compact on its first turn instead of waiting one turn for real usage.
        let estimate = compaction::estimate_tokens(&rollout.project());
        session.last_usage = TokenUsage {
            input_tokens: estimate,
            ..TokenUsage::default()
        };
        session.rollout = rollout;
        session
    }

    /// Install a step hook. Hooks compose: each runs in registration order on
    /// every model round-trip, each seeing the previous hooks' edits.
    pub fn add_hook(&mut self, hook: Arc<dyn StepHook>) {
        self.hooks.push(hook);
    }

    /// The effective history `E(L)` — the messages the model would be given if a
    /// turn started now (post-compaction, post-rewind). Owned: it is a
    /// projection of the log, not a field.
    pub fn messages(&self) -> Vec<Message> {
        self.rollout.project()
    }

    /// The underlying append-only log, for hosts that persist or fork it.
    pub fn rollout(&self) -> &Rollout {
        &self.rollout
    }

    /// A handle for submitting mid-turn input to whatever turn is running on
    /// this session. Obtain it before starting the turn; use it from another
    /// task while `run_turn` executes ([docs/ac-queue-steer.md]).
    pub fn steer_handle(&self) -> SteerHandle {
        SteerHandle::new(self.steer.clone())
    }

    fn record(&mut self, msg: Message) {
        self.rollout.record_message(msg);
    }

    fn next_turn(&mut self) -> u64 {
        self.turn_counter += 1;
        self.turn_counter
    }

    /// Move the active turn's pending steers into history as plain user
    /// messages, unsampled — the terminal-flush of [docs/ac-queue-steer.md] §4:
    /// input the runtime accepted reaches history even when the turn ends
    /// abnormally (R2), except under deliberate cancellation ([`on_user_cancel`]).
    fn flush_pending(&mut self) {
        for item in self.steer.take_pending() {
            match item {
                SteerInput::Text(t) => self.record(Message::text(Role::User, t)),
            }
        }
    }

    /// Deliberate cancellation ([docs/ac-queue-steer.md] §5): discard the
    /// pending queue (the user said stop, including what they just typed),
    /// record the interruption marker so the next turn's model reads the cut as
    /// intentional, and close the turn — a cancelled turn is self-documented, so
    /// a later fork sees a clean boundary, not a ragged edge to re-mark.
    fn on_user_cancel(&mut self, turn_no: u64) {
        let _ = self.steer.take_pending();
        self.record(Message::text(Role::User, INTERRUPTION_MARKER));
        self.rollout.end_turn(turn_no);
    }

    /// Whether the measured context occupancy has reached the compaction budget.
    /// Always false when compaction is unconfigured.
    fn over_budget(&self) -> bool {
        match &self.config.compaction {
            Some(cfg) => {
                compaction::context_occupancy(&self.last_usage, cfg.exclude_cached_prefix)
                    >= cfg.budget_tokens
            }
            None => false,
        }
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

        let turn_no = self.next_turn();
        self.rollout.start_turn(turn_no);
        self.record(Message::text(Role::User, user_text));

        // The turn's own input (I₀) must sample before any steer, so draining
        // is deferred (`drainable = false`) until the first step completes; the
        // guard deactivates the turn on every exit, including a panic.
        let turn_id = self.steer.activate(TurnClass::Regular);
        let _turn_guard = ActiveTurnGuard {
            state: self.steer.clone(),
            id: turn_id.clone(),
        };
        let mut drainable = false;
        // After a mid-turn compaction the model must re-establish against `H′`
        // before new user intent lands, so the next step's drain is skipped once
        // ([docs/ac-compaction.md] §5, [docs/ac-queue-steer.md] §4).
        let mut defer_drain_once = false;

        // Pre-turn trigger: clear the runway before the first step.
        if self.over_budget() {
            match self
                .compact_inner(CompactionTrigger::PreTurn, &cancel, &provider, &sink)
                .await
            {
                Ok(_) | Err(CompactionError::NothingToCompact) => {}
                Err(CompactionError::Cancelled) => {
                    self.on_user_cancel(turn_no);
                    return Err(RuntimeError::Cancelled);
                }
                Err(e) => {
                    self.flush_pending();
                    return Err(RuntimeError::Compaction(e));
                }
            }
        }

        let mut iteration = 0usize;
        loop {
            if iteration >= self.config.max_iterations {
                self.flush_pending();
                return Err(RuntimeError::MaxIterations(self.config.max_iterations));
            }
            if cancel.is_cancelled() {
                self.on_user_cancel(turn_no);
                return Err(RuntimeError::Cancelled);
            }
            // A dropped receiver means nobody is listening — treat it as an
            // implicit cancel so we stop spending tokens and running tools.
            // Not deliberate user intent: discard the queue, but record no
            // interruption marker (the client simply went away) and leave the
            // turn open — a ragged edge a later fork/resume marks.
            if sink.is_closed() {
                let _ = self.steer.take_pending();
                return Err(RuntimeError::Cancelled);
            }

            // Step boundary: drain pending steers into history as plain user
            // messages ([docs/ac-queue-steer.md] §4). `drainable` gates the
            // initial deferral; `defer_drain_once` gates the post-compaction one.
            if drainable && !defer_drain_once {
                for item in self.steer.take_pending() {
                    match item {
                        SteerInput::Text(t) => self.record(Message::text(Role::User, t)),
                    }
                }
            }
            defer_drain_once = false;

            let mut req = CompletionRequest::new(&self.config.model);
            req.system = self.config.system.clone();
            req.cache_system = self.config.system.is_some();
            req.messages = self.rollout.project();
            req.tools = self.registry.specs();
            req.server_tools = self.config.server_tools.clone();

            for hook in &self.hooks {
                hook.prepare(iteration, &mut req);
            }

            // Await the connection, but let a cancel break out of it.
            let mut stream = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    self.on_user_cancel(turn_no);
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
                        self.on_user_cancel(turn_no);
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
                        self.last_usage = u;
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
            self.record(Message {
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
                    self.rollout.end_turn(turn_no);
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
            self.record(Message {
                role: Role::User,
                content: user_content,
                cache: false,
            });

            // Mid-turn trigger: the model owes a continuation (tool calls just
            // ran) and `τ ≥ β`. Checkpoint, then continue the same turn — the
            // model re-establishes its interrupted work against `H′` next step.
            if self.over_budget() {
                match self
                    .compact_inner(CompactionTrigger::MidTurn, &cancel, &provider, &sink)
                    .await
                {
                    Ok(_) => defer_drain_once = true,
                    Err(CompactionError::NothingToCompact) => {}
                    Err(CompactionError::Cancelled) => {
                        self.on_user_cancel(turn_no);
                        return Err(RuntimeError::Cancelled);
                    }
                    Err(e) => {
                        self.flush_pending();
                        return Err(RuntimeError::Compaction(e));
                    }
                }
            }

            iteration += 1;
        }
    }

    /// Compact the session's context now, on demand ([docs/ac-compaction.md],
    /// manual trigger). Call it between turns — the `&mut self` borrow already
    /// guarantees no turn is running. The compaction turn is non-steerable
    /// ([docs/ac-queue-steer.md] §3): it is activated as [`TurnClass::Compaction`]
    /// for its duration so a concurrent steer is refused, not absorbed.
    pub async fn compact(
        &mut self,
        sink: &UnboundedSender<AgentEvent>,
    ) -> Result<CompactionOutcome, CompactionError> {
        let cancel = self.ctx.cancel.clone();
        let provider = self.provider.clone();
        let turn_id = self.steer.activate(TurnClass::Compaction);
        let _guard = ActiveTurnGuard {
            state: self.steer.clone(),
            id: turn_id,
        };
        self.compact_inner(CompactionTrigger::Manual, &cancel, &provider, sink)
            .await
    }

    /// The transformation `C : H → H′` and its record. Shared by all three
    /// triggers so they are one code path (R4): produce `σ`, build
    /// `H′ = U ⧺ ⟨σ⟩`, append the `κ` record, reset `τ`, emit the event.
    async fn compact_inner(
        &mut self,
        trigger: CompactionTrigger,
        cancel: &CancellationToken,
        provider: &Arc<dyn Provider>,
        sink: &UnboundedSender<AgentEvent>,
    ) -> Result<CompactionOutcome, CompactionError> {
        let cfg = self
            .config
            .compaction
            .clone()
            .ok_or(CompactionError::Disabled)?;

        let view = self.rollout.project();
        let messages_before = view.len();
        let tokens_before =
            compaction::context_occupancy(&self.last_usage, cfg.exclude_cached_prefix);

        // Nothing to compress if the view is only user input — `H′` would equal
        // `H`. The caller proceeds uncompacted.
        if !view.iter().any(|m| !compaction::is_user_input(m)) {
            return Err(CompactionError::NothingToCompact);
        }

        let summary = match cfg.strategy {
            CompactionStrategy::FreshWindow => String::new(),
            CompactionStrategy::Summarize => {
                self.run_summary(cancel, provider, &cfg, &view).await?
            }
        };

        let u = compaction::survivors(&view, cfg.per_message_cap_tokens, &self.fragments);
        let replacement = compaction::build_replacement(u, &summary, cfg.strategy);
        let tokens_after = compaction::estimate_tokens(&replacement);

        // R3: if `C` did not clear the budget, it would re-trigger immediately —
        // surface an error rather than loop.
        if tokens_after >= cfg.budget_tokens {
            return Err(CompactionError::Ineffective {
                budget: cfg.budget_tokens,
                achieved: tokens_after,
            });
        }

        let messages_after = replacement.len();
        self.rollout
            .compact(summary.clone(), trigger.as_str(), replacement);
        // Reset `τ` to the estimate so a stale pre-compaction figure cannot
        // re-fire a trigger before the next real usage lands.
        self.last_usage = TokenUsage {
            input_tokens: tokens_after,
            ..TokenUsage::default()
        };

        let _ = sink.send(AgentEvent::Compacted {
            trigger: trigger.as_str().to_string(),
            summary: summary.clone(),
            tokens_before,
            tokens_after,
        });

        Ok(CompactionOutcome {
            trigger,
            strategy: cfg.strategy,
            summary_chars: summary.len(),
            tokens_before,
            tokens_after,
            messages_before,
            messages_after,
        })
    }

    /// Produce `σ` under the handoff contract (R1): one non-tool round-trip over
    /// the current view, collecting the model's text. Honors cancellation and
    /// the idle timeout, like a normal step.
    async fn run_summary(
        &self,
        cancel: &CancellationToken,
        provider: &Arc<dyn Provider>,
        cfg: &CompactionConfig,
        view: &[Message],
    ) -> Result<String, CompactionError> {
        let system = cfg
            .handoff_system
            .clone()
            .unwrap_or_else(|| compaction::HANDOFF_SYSTEM.to_string());
        let req = compaction::build_summary_request(
            &self.config.model,
            system,
            view.to_vec(),
            cfg.summary_max_tokens,
        );

        let mut stream = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(CompactionError::Cancelled),
            res = provider.stream_completion(req) => res?,
        };

        let mut summary = String::new();
        loop {
            let next = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(CompactionError::Cancelled),
                n = async {
                    match self.config.idle_timeout {
                        Some(d) => tokio::time::timeout(d, stream.next()).await.map_err(|_| ()),
                        None => Ok(stream.next().await),
                    }
                } => n,
            };
            match next {
                Err(()) => return Err(CompactionError::Timeout),
                Ok(None) => break,
                Ok(Some(Ok(CompletionEvent::Text(s)))) => summary.push_str(&s),
                Ok(Some(Ok(CompletionEvent::Stop(_)))) => break,
                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(e))) => return Err(CompactionError::Completion(e)),
            }
        }
        Ok(summary)
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
            AgentEvent::Compacted {
                trigger: "mid_turn".into(),
                summary: "handoff".into(),
                tokens_before: 1000,
                tokens_after: 50,
            },
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
