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
mod history;
mod hooks;
mod spawn;
mod steer;

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use ac_context::{FragmentRegistry, ReactiveSection, reactive_fragment};
use ac_provider::{CompletionRequest, Provider, ServerTool};
use ac_rollout::Rollout;
use ac_tool::{ToolCtx, ToolOutput, ToolOutputPart, ToolRegistry};
use ac_types::{
    CacheMark, CompletionError, CompletionEvent, ContentPart, Effort, Message, Role, StopReason,
    TokenUsage, ToolResult, ToolUse,
};
use futures::StreamExt;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

pub use compaction::{
    CompactionConfig, CompactionError, CompactionOutcome, CompactionStrategy, CompactionTrigger,
};
pub use history::{
    ABORTED_TOOL_RESULT, is_contentless, repair_dangling_tool_uses, sanitize_messages,
};
pub use hooks::{
    BoundedForcedChainHook, ConditionalToolsHook, FirstStepServerToolsOnly, ForcedChainHook,
    HookRegistry, Observation, ObservationHook, StepPrepareHook, TailCacheHook,
};
pub use spawn::ReferenceSpawner;
pub use steer::{SteerError, SteerHandle, SteerInput, TurnClass};

use steer::SteerState;

pub use ac_types::INTERRUPTION_MARKER;

/// Default upper bound for live-only tool result data retained during one turn.
///
/// This is intentionally independent of the durable rollout: when the budget
/// evicts an entry, the next request simply sees that call's recorded fallback.
pub const DEFAULT_TRANSIENT_TOOL_OUTPUT_BYTES: usize = 128 * 1024 * 1024;

/// Deactivates the active turn when the turn's scope ends, on every exit path
/// including a panic unwind — so a stale active turn never outlives its
/// `run_turn`.
struct ActiveTurnGuard {
    state: Arc<SteerState>,
    id: String,
}

/// A tool result's live form, retained only while its producing turn runs.
///
/// The rollout contains the tool's durable fallback. On the next provider
/// sample, matching results are overlaid with this content and followed by user
/// image messages. Keeping this map inside `run_turn` makes the
/// persistence boundary structural: a later turn, resume, or fork cannot
/// accidentally replay base64 payloads.
#[derive(Debug)]
struct TransientToolOutput {
    content: String,
    parts: Vec<ToolOutputPart>,
}

impl TransientToolOutput {
    fn retained_bytes(&self) -> usize {
        self.content.len()
            + self
                .parts
                .iter()
                .map(|part| match part {
                    ToolOutputPart::Image { media_type, data } => media_type.len() + data.len(),
                })
                .sum::<usize>()
    }
}

/// FIFO-bounded turn-local outputs, keyed by provider tool-call id.
struct TransientToolOutputs {
    entries: HashMap<String, TransientToolOutput>,
    order: VecDeque<String>,
    retained_bytes: usize,
    max_bytes: usize,
}

impl TransientToolOutputs {
    fn new(max_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            retained_bytes: 0,
            max_bytes,
        }
    }

    fn insert(&mut self, id: String, output: TransientToolOutput) {
        if let Some(previous) = self.entries.remove(&id) {
            self.retained_bytes = self
                .retained_bytes
                .saturating_sub(previous.retained_bytes());
            self.order.retain(|key| key != &id);
        }
        self.retained_bytes = self.retained_bytes.saturating_add(output.retained_bytes());
        self.order.push_back(id.clone());
        self.entries.insert(id, output);

        while self.retained_bytes > self.max_bytes {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(evicted) = self.entries.remove(&oldest) {
                self.retained_bytes = self.retained_bytes.saturating_sub(evicted.retained_bytes());
            }
        }
    }

    /// Forget outputs after their one eligible provider sample.
    fn consume(&mut self, ids: &HashSet<String>) {
        if ids.is_empty() {
            return;
        }
        for id in ids {
            if let Some(output) = self.entries.remove(id) {
                self.retained_bytes = self.retained_bytes.saturating_sub(output.retained_bytes());
            }
        }
        self.order.retain(|id| !ids.contains(id));
    }
}

/// Project live tool output onto the request before host step hooks run.
///
/// A provider may encode all `ToolResult`s from one AC user message as a run of
/// `role:"tool"` wire messages. Images therefore follow the whole result
/// message, preserving the required `assistant(tool_calls) -> tool* -> user`
/// ordering when several calls ran concurrently.
fn overlay_transient_tool_outputs(
    messages: &mut Vec<Message>,
    outputs: &HashMap<String, TransientToolOutput>,
) -> HashSet<String> {
    let mut overlaid = HashSet::new();
    if outputs.is_empty() {
        return overlaid;
    }

    let mut index = 0usize;
    while index < messages.len() {
        let mut images = Vec::new();
        for part in &mut messages[index].content {
            let ContentPart::ToolResult(result) = part else {
                continue;
            };
            let Some(output) = outputs.get(&result.tool_use_id) else {
                continue;
            };
            overlaid.insert(result.tool_use_id.clone());
            result.content.clone_from(&output.content);
            for part in &output.parts {
                match part {
                    ToolOutputPart::Image { media_type, data } => {
                        images.push(ContentPart::Image {
                            media_type: media_type.clone(),
                            data: data.to_string(),
                        });
                    }
                }
            }
        }

        if !images.is_empty() {
            messages.insert(
                index + 1,
                Message {
                    role: Role::User,
                    content: images,
                    cache: CacheMark::Off,
                },
            );
            index += 1;
        }
        index += 1;
    }
    overlaid
}

/// Reconstruct only the transient calls from a tool step that compaction is
/// about to replace.
///
/// The returned messages contain durable result bodies. They live only in
/// `run_turn`; the ordinary transient projection overlays their live content
/// immediately before the next provider sample. Replaying the matching
/// `ToolUse` alongside each result preserves provider call/result ordering
/// without putting either the replay or its pixels into the rollout.
fn compacted_transient_replay(
    tool_uses: &[ToolUse],
    tool_results: &[ContentPart],
    outputs: &HashMap<String, TransientToolOutput>,
) -> Vec<Message> {
    if outputs.is_empty() {
        return Vec::new();
    }

    let uses: Vec<ContentPart> = tool_uses
        .iter()
        .filter(|tool_use| outputs.contains_key(&tool_use.id))
        .cloned()
        .map(ContentPart::ToolUse)
        .collect();
    if uses.is_empty() {
        return Vec::new();
    }

    let results: Vec<ContentPart> = tool_results
        .iter()
        .filter(|part| {
            matches!(
                part,
                ContentPart::ToolResult(result) if outputs.contains_key(&result.tool_use_id)
            )
        })
        .cloned()
        .collect();
    if results.is_empty() {
        return Vec::new();
    }

    vec![
        Message {
            role: Role::Assistant,
            content: uses,
            cache: CacheMark::Off,
        },
        Message {
            role: Role::User,
            content: results,
            cache: CacheMark::Off,
        },
    ]
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
    /// Maximum number of contentless `EndTurn` responses retried during one
    /// turn. Thinking, citations, usage, and whitespace do not count as
    /// assistant output. Defaults to one retry.
    pub empty_completion_retries: usize,
    /// Context-compaction budget and policy ([docs/ac-compaction.md]). `None`
    /// disables compaction: no trigger fires and manual `compact` is refused.
    pub compaction: Option<CompactionConfig>,
    /// Default reasoning-effort tier applied to every request ([docs/ac-ultra.md]
    /// §3). A default, not a freeze — a step-prepare hook may override it per
    /// step. `None` uses the provider's default.
    pub effort: Option<Effort>,
    /// Maximum live-only tool-output bytes retained during one turn. Oldest
    /// call ids are evicted first; their durable results remain intact.
    /// Defaults to 128 MiB. Set to zero to disable transient replay.
    pub transient_tool_output_bytes: usize,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            system: None,
            max_iterations: 16,
            server_tools: Vec::new(),
            idle_timeout: Some(Duration::from_secs(300)),
            empty_completion_retries: 1,
            compaction: None,
            effort: None,
            transient_tool_output_bytes: DEFAULT_TRANSIENT_TOOL_OUTPUT_BYTES,
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
    /// Accepted mid-turn input has crossed a durable history boundary.
    ///
    /// Emitted immediately after the plain user message is appended to the
    /// rollout, whether at an ordinary step drain or the non-cancel terminal
    /// flush. Initial turn input, runtime-authored user rows, and cancellation
    /// markers do not emit this event.
    InputCommitted {
        message: Message,
    },
    ToolCall {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// A raw fragment of a tool call's arguments, streamed as the provider
    /// emits them. Stream-only (like `Thinking`, the stream carries strictly
    /// more than history): it never enters the rollout, and the assembled
    /// `ToolCall` that follows remains the single authoritative call.
    ToolInputDelta {
        id: String,
        name: String,
        delta: String,
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

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("completion failed: {0}")]
    Completion(#[from] ac_types::CompletionError),
    #[error("exceeded max iterations ({0})")]
    MaxIterations(usize),
    #[error("provider stalled: no event within the idle timeout")]
    Timeout,
    #[error("provider completed without assistant text or tool calls ({0:?})")]
    EmptyCompletion(StopReason),
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
    hooks: HookRegistry,
    rollout: Rollout,
    /// The most recent server-reported usage — the source of truth for `τ`.
    last_usage: TokenUsage,
    /// Session-monotonic turn numbering (fork cut points).
    turn_counter: u64,
    /// Recognizes the runtime's own machine-injected fragments ([docs/ac-context.md]),
    /// so they are filtered from user input rather than promoted to instructions.
    fragments: FragmentRegistry,
    /// Host-registered reactive context sections ([docs/ac-context.md] §5,
    /// [docs/ac-ultra.md] §4): each emits a marked fragment at a boundary only
    /// when its render differs from the last one recognized in `E(L)`.
    reactive: Vec<Arc<dyn ReactiveSection>>,
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
            hooks: HookRegistry::new(),
            rollout: Rollout::create(),
            last_usage: TokenUsage::default(),
            turn_counter: 0,
            fragments: fragments::runtime_registry(),
            reactive: Vec::new(),
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
        mut history: Vec<Message>,
    ) -> Self {
        sanitize_messages(&mut history);
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
        let mut model_history = rollout.project();
        sanitize_messages(&mut model_history);
        let estimate = compaction::estimate_tokens(&model_history);
        session.last_usage = TokenUsage {
            input_tokens: estimate,
            ..TokenUsage::default()
        };
        session.rollout = rollout;
        session
    }

    /// Install a step-prepare hook ([docs/ac-hooks.md]). They compose: each runs
    /// in registration order on every model round-trip, each seeing the previous
    /// hooks' edits.
    pub fn add_step_hook(&mut self, hook: Arc<dyn StepPrepareHook>) {
        self.hooks.add_step_prepare(hook);
    }

    /// Install an observation hook — it watches tool traffic and contributes
    /// nothing (I4/I6). Registration order is fixed but nothing model-visible may
    /// depend on it.
    pub fn add_observation_hook(&mut self, hook: Arc<dyn ObservationHook>) {
        self.hooks.add_observation(hook);
    }

    /// Install a reactive context section ([docs/ac-context.md] §5,
    /// [docs/ac-ultra.md] §4). Its fragment class is registered so the section's
    /// emissions are recognized (stripped on compaction, filtered from user
    /// input); the driver then evaluates it at each turn boundary and window
    /// re-establishment, appending its fragment only when it changed.
    pub fn add_reactive_section(&mut self, section: Arc<dyn ReactiveSection>) {
        self.fragments.register(section.class().clone());
        self.reactive.push(section);
    }

    /// Drive every reactive section against the current effective history,
    /// appending the fragment of any whose render differs from the last one
    /// recognized in `E(L)`. Prior is read from the log, so a compaction strip
    /// re-injects the section into the new window and a resume/fork continues the
    /// logged value — no retained snapshot.
    fn drive_reactive(&mut self) {
        if self.reactive.is_empty() {
            return;
        }
        let sections = self.reactive.clone();
        for section in sections {
            let history = self.model_messages();
            let texts: Vec<String> = history.iter().map(compaction::message_text).collect();
            let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
            if let Some(fragment) = reactive_fragment(section.as_ref(), &refs) {
                self.record(Message::text(Role::User, fragment));
            }
        }
    }

    /// The effective history `E(L)` — the messages the model would be given if a
    /// turn started now (post-compaction, post-rewind). Owned: it is a
    /// projection of the log, not a field.
    pub fn messages(&self) -> Vec<Message> {
        self.model_messages()
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

    /// Provider-safe projection of the effective history.
    ///
    /// New turns satisfy these invariants at the source; sanitization remains
    /// here so a legacy/imported full rollout cannot poison every future
    /// request.
    fn model_messages(&self) -> Vec<Message> {
        let mut messages = self.rollout.project();
        sanitize_messages(&mut messages);
        messages
    }

    fn next_turn(&mut self) -> u64 {
        self.turn_counter += 1;
        self.turn_counter
    }

    /// Append one accepted mid-turn input as a plain user message and publish
    /// its live checkpoint only after the durable rollout mutation.
    fn commit_pending_input(&mut self, item: SteerInput, sink: &UnboundedSender<AgentEvent>) {
        let message = match item {
            SteerInput::Text(t) => Message::text(Role::User, t),
        };
        self.record(message.clone());
        let _ = sink.send(AgentEvent::InputCommitted { message });
    }

    /// Move the active turn's pending steers into history, unsampled — the
    /// terminal flush of [docs/ac-queue-steer.md] §4. Input the runtime
    /// accepted reaches history even when the turn ends abnormally (R2),
    /// except under deliberate cancellation ([`on_user_cancel`]).
    fn flush_pending(&mut self, sink: &UnboundedSender<AgentEvent>) {
        for item in self.steer.take_pending() {
            self.commit_pending_input(item, sink);
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
        // Live-only tool payloads belong to this invocation of `run_turn`, not
        // to Session state. The next user turn and every resumed/forked session
        // see only the durable results recorded in the rollout.
        let mut transient_tool_outputs =
            TransientToolOutputs::new(self.config.transient_tool_output_bytes);
        // A mid-turn compaction removes the just-recorded tool exchange from
        // H′. Keep a durable-only reconstruction here until the next ordinary
        // provider sample can receive the transient overlay exactly once.
        let mut pending_compacted_transient_replay = Vec::new();

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
                    self.flush_pending(&sink);
                    return Err(RuntimeError::Compaction(e));
                }
            }
        }

        let mut iteration = 0usize;
        let mut empty_completion_retries = 0usize;
        loop {
            if iteration >= self.config.max_iterations {
                self.flush_pending(&sink);
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
                    self.commit_pending_input(item, &sink);
                }
            }
            defer_drain_once = false;

            // Reactive sections fire at the step boundary (the same point the
            // steer queue drains): each appends its marked fragment only when its
            // render differs from the last recognized in the log — so a mode
            // injects at session start, re-injects after a compaction stripped
            // it, and emits once on a mid-turn flip, staying silent otherwise.
            self.drive_reactive();

            let mut req = CompletionRequest::new(&self.config.model);
            req.system = self.config.system.clone();
            req.cache_system = self.config.system.is_some().into();
            req.messages = self.model_messages();
            req.tools = self.registry.specs();
            req.server_tools = self.config.server_tools.clone();
            req.effort = self.config.effort;

            // A successful mid-turn compaction has removed the matching
            // call/result pair from the durable projection. Restore that pair
            // only in this outgoing request so the existing overlay can attach
            // live content and images without persisting them.
            req.messages.append(&mut pending_compacted_transient_replay);

            // Transient results are an ephemeral projection over the durable
            // rollout. Apply them before hooks so step-prepare remains the
            // final authority over the outgoing request (redaction included).
            let overlaid =
                overlay_transient_tool_outputs(&mut req.messages, &transient_tool_outputs.entries);

            // Step-prepare hooks fold last, so a hook MAY override the effort
            // default per step ([docs/ac-ultra.md] §3, [docs/ac-hooks.md]).
            for hook in self.hooks.step_prepare() {
                hook.prepare(iteration, &mut req);
            }
            // The request now owns its cloned live payload. Drop the turn-local
            // source before sampling so no later same-turn request can replay
            // an already-offered image or live result.
            transient_tool_outputs.consume(&overlaid);
            // The exact local-tool authority granted to this sampling request.
            // A provider-emitted call to a registered-but-filtered tool must
            // fail as data instead of bypassing a visibility/permission hook.
            let offered_tools: Arc<HashSet<String>> =
                Arc::new(req.tools.iter().map(|tool| tool.name.clone()).collect());

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
                        self.flush_pending(&sink);
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
                        self.flush_pending(&sink);
                        return Err(RuntimeError::Timeout);
                    }
                    Ok(None) => break,
                    Ok(Some(Ok(ev))) => ev,
                    Ok(Some(Err(e))) => {
                        self.flush_pending(&sink);
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
                    CompletionEvent::ToolCallDelta {
                        id,
                        name,
                        args_delta,
                    } => {
                        let _ = sink.send(AgentEvent::ToolInputDelta {
                            id,
                            name,
                            delta: args_delta,
                        });
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
            if !text.trim().is_empty() {
                assistant_content.push(ContentPart::Text { text });
            }
            for tu in &tool_uses {
                assistant_content.push(ContentPart::ToolUse(tu.clone()));
            }
            if assistant_content.is_empty() {
                // Thinking, citations, and usage are observational stream data;
                // none is a durable assistant response the next request can
                // replay. Retry a nominal EndTurn within the configured
                // per-turn budget; the unchanged history makes this a clean
                // provider retry, while the ordinary iteration bound remains
                // the outer safety net. A completed empty step still opens the
                // steer drain for the retry.
                if stop_reason == StopReason::EndTurn
                    && empty_completion_retries < self.config.empty_completion_retries
                {
                    empty_completion_retries += 1;
                    drainable = true;
                    iteration += 1;
                    continue;
                }
                // Once the retry budget is exhausted (or the provider reports a
                // different stop reason), surface a machinery failure instead
                // of successful completion. Preserve any steer that raced this
                // sampling request using the same terminal-flush discipline as
                // other provider failures.
                self.flush_pending(&sink);
                return Err(RuntimeError::EmptyCompletion(stop_reason));
            }
            self.record(Message {
                role: Role::Assistant,
                content: assistant_content,
                cache: CacheMark::Off,
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
                self.hooks.observe(&Observation::ToolStart {
                    id: tu.id.clone(),
                    name: tu.name.clone(),
                });
                let registry = self.registry.clone();
                // Every concurrent dispatch gets the exact provider call id in
                // its own shallow context clone. Run-scoped policy/state remain
                // shared; invocation identity cannot be reordered by task
                // scheduling or observation hooks.
                let ctx = Arc::new(self.ctx.for_invocation(tu.id.clone()));
                let name = tu.name.clone();
                let input = tu.input.clone();
                let offered_tools = offered_tools.clone();
                let handle = tokio::spawn(async move {
                    if !offered_tools.contains(&name) {
                        ToolOutput::error(format!(
                            "tool '{name}' was not available on this step; use an offered discovery tool first"
                        ))
                    } else {
                        registry.run(&name, input, ctx).await
                    }
                });
                handles.push((tu.id.clone(), tu.name.clone(), handle));
            }

            let mut user_content: Vec<ContentPart> = Vec::with_capacity(handles.len());
            let mut handles = handles.into_iter();
            while let Some((id, name, mut handle)) = handles.next() {
                let joined = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        handle.abort();
                        let mut aborted = vec![(id.clone(), name.clone())];
                        for (pending_id, pending_name, pending_handle) in handles {
                            pending_handle.abort();
                            aborted.push((pending_id, pending_name));
                        }
                        for (aborted_id, aborted_name) in aborted {
                            self.hooks.observe(&Observation::ToolFinish {
                                id: aborted_id.clone(),
                                name: aborted_name.clone(),
                                is_error: true,
                            });
                            let _ = sink.send(AgentEvent::ToolResult {
                                id: aborted_id.clone(),
                                name: aborted_name,
                                output: ABORTED_TOOL_RESULT.to_string(),
                                is_error: true,
                            });
                            user_content.push(ContentPart::ToolResult(ToolResult {
                                tool_use_id: aborted_id,
                                content: ABORTED_TOOL_RESULT.to_string(),
                                is_error: true,
                            }));
                        }
                        // Close every call before recording the interruption
                        // marker. A cancelled turn is immediately valid
                        // provider history, not something each host must heal
                        // while reloading it.
                        self.record(Message {
                            role: Role::User,
                            content: user_content,
                            cache: CacheMark::Off,
                        });
                        self.on_user_cancel(turn_no);
                        return Err(RuntimeError::Cancelled);
                    }
                    joined = &mut handle => joined,
                };
                let (content, durable_content, transient_parts, is_error) = match joined {
                    Ok(out) => {
                        let durable = out.durable_content.unwrap_or_else(|| out.content.clone());
                        (out.content, durable, out.transient_parts, out.is_error)
                    }
                    Err(e) => {
                        let content = format!("tool '{name}' panicked: {e}");
                        (content.clone(), content, Vec::new(), true)
                    }
                };
                if content != durable_content || !transient_parts.is_empty() {
                    transient_tool_outputs.insert(
                        id.clone(),
                        TransientToolOutput {
                            content: content.clone(),
                            parts: transient_parts,
                        },
                    );
                }
                self.hooks.observe(&Observation::ToolFinish {
                    id: id.clone(),
                    name: name.clone(),
                    is_error,
                });
                let _ = sink.send(AgentEvent::ToolResult {
                    id: id.clone(),
                    name,
                    output: content.clone(),
                    is_error,
                });
                user_content.push(ContentPart::ToolResult(ToolResult {
                    tool_use_id: id,
                    content: durable_content,
                    is_error,
                }));
            }
            let replay_after_compaction = compacted_transient_replay(
                &tool_uses,
                &user_content,
                &transient_tool_outputs.entries,
            );
            self.record(Message {
                role: Role::User,
                content: user_content,
                cache: CacheMark::Off,
            });

            // Mid-turn trigger: the model owes a continuation (tool calls just
            // ran) and `τ ≥ β`. Checkpoint, then continue the same turn — the
            // model re-establishes its interrupted work against `H′` next step.
            if self.over_budget() {
                match self
                    .compact_inner(CompactionTrigger::MidTurn, &cancel, &provider, &sink)
                    .await
                {
                    Ok(_) => {
                        defer_drain_once = true;
                        pending_compacted_transient_replay = replay_after_compaction;
                    }
                    Err(CompactionError::NothingToCompact) => {}
                    Err(CompactionError::Cancelled) => {
                        self.on_user_cancel(turn_no);
                        return Err(RuntimeError::Cancelled);
                    }
                    Err(e) => {
                        self.flush_pending(&sink);
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

        let view = self.model_messages();
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

        collect_completion_text(
            provider.as_ref(),
            req,
            Some(cancel),
            self.config.idle_timeout,
        )
        .await
        .map_err(|e| match e {
            CollectTextError::Cancelled => CompactionError::Cancelled,
            CollectTextError::Timeout => CompactionError::Timeout,
            CollectTextError::Completion(e) => CompactionError::Completion(e),
        })
    }
}

/// Why [`collect_completion_text`] gave up before producing a string.
#[derive(Debug, thiserror::Error)]
pub enum CollectTextError {
    #[error("cancelled")]
    Cancelled,
    /// No event arrived within the idle timeout — a stalled provider, not a
    /// deliberate cancel.
    #[error("stalled: no event within the idle timeout")]
    Timeout,
    #[error(transparent)]
    Completion(#[from] CompletionError),
}

/// Drive one completion to a single concatenated string of its text events —
/// the one-shot path for short utility completions (titling, classification,
/// summarization) where the caller wants a `String`, not a stream or a
/// session. Non-text events are ignored; the collection ends at `Stop` or
/// stream end.
///
/// `cancel` aborts between events and `idle_timeout` bounds the wait for the
/// next one; pass `None` for either to opt out.
pub async fn collect_completion_text(
    provider: &dyn Provider,
    request: CompletionRequest,
    cancel: Option<&CancellationToken>,
    idle_timeout: Option<Duration>,
) -> Result<String, CollectTextError> {
    let never = CancellationToken::new();
    let cancel = cancel.unwrap_or(&never);

    let mut stream = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(CollectTextError::Cancelled),
        res = provider.stream_completion(request) => res?,
    };

    let mut text = String::new();
    loop {
        let next = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(CollectTextError::Cancelled),
            n = async {
                match idle_timeout {
                    Some(d) => tokio::time::timeout(d, stream.next()).await.map_err(|_| ()),
                    None => Ok(stream.next().await),
                }
            } => n,
        };
        match next {
            Err(()) => return Err(CollectTextError::Timeout),
            Ok(None) => break,
            Ok(Some(Ok(CompletionEvent::Text(s)))) => text.push_str(&s),
            Ok(Some(Ok(CompletionEvent::Stop(_)))) => break,
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(e))) => return Err(CollectTextError::Completion(e)),
        }
    }
    Ok(text)
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
            AgentEvent::InputCommitted {
                message: Message::text(Role::User, "changed direction"),
            },
            AgentEvent::ToolCall {
                id: "c1".into(),
                name: "read_file".into(),
                input: serde_json::json!({ "path": "a.txt" }),
            },
            AgentEvent::ToolInputDelta {
                id: "c1".into(),
                name: "read_file".into(),
                delta: "{\"pa".into(),
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
