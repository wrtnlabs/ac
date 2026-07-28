//! Application-agnostic composition helpers for hosting [`ac_runtime`].
//!
//! This crate owns no prompt, tool set, policy, transport, or persistence
//! decision. It only removes two pieces of wiring that every host would
//! otherwise reimplement:
//!
//! - [`AgentHostBuilder`] assembles a fresh or resumed [`Session`] from objects
//!   the host already chose, installing lifecycle contributors in declaration
//!   order.
//! - [`TurnPump`] borrows that session for one turn and exposes the runtime's
//!   ordered events followed by exactly one terminal result.
//!
//! It is the framework's composition harness, not an application framework or
//! orchestration DSL: there are no chains, graphs, prompt abstractions, domain
//! profiles, or transport opinions here.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use ac_context::ReactiveSection;
use ac_provider::Provider;
use ac_rollout::Rollout;
use ac_runtime::{
    AgentConfig, AgentEvent, ObservationHook, RuntimeError, Session, StepPrepareHook,
};
use ac_tool::{ToolCtx, ToolRegistry};
use ac_types::{Message, StopReason};
use tokio::sync::mpsc::{UnboundedReceiver, error::TryRecvError, unbounded_channel};

enum SessionSource {
    Fresh,
    Messages(Vec<Message>),
    Rollout(Rollout),
}

/// Declarative assembly for one AC [`Session`].
///
/// The required objects are deliberately injected rather than defaulted. A
/// host remains the sole owner of provider choice, tool contracts, path and
/// sandbox policy, prompt text, model settings, and resume source.
pub struct AgentHostBuilder {
    provider: Arc<dyn Provider>,
    registry: Arc<ToolRegistry>,
    ctx: Arc<ToolCtx>,
    config: AgentConfig,
    source: SessionSource,
    step_hooks: Vec<Arc<dyn StepPrepareHook>>,
    observation_hooks: Vec<Arc<dyn ObservationHook>>,
    reactive_sections: Vec<Arc<dyn ReactiveSection>>,
}

impl AgentHostBuilder {
    /// Begin assembly of a fresh session.
    pub fn new(
        provider: Arc<dyn Provider>,
        registry: Arc<ToolRegistry>,
        ctx: Arc<ToolCtx>,
        config: AgentConfig,
    ) -> Self {
        Self::with_source(provider, registry, ctx, config, SessionSource::Fresh)
    }

    /// Begin assembly from a host-persisted flat message projection.
    ///
    /// This has the same recovery semantics as [`Session::resume`]. Prefer
    /// [`resume_from`](Self::resume_from) when the full rollout is available.
    pub fn resume(
        provider: Arc<dyn Provider>,
        registry: Arc<ToolRegistry>,
        ctx: Arc<ToolCtx>,
        config: AgentConfig,
        history: Vec<Message>,
    ) -> Self {
        Self::with_source(
            provider,
            registry,
            ctx,
            config,
            SessionSource::Messages(history),
        )
    }

    /// Begin full-fidelity assembly from a persisted rollout.
    pub fn resume_from(
        provider: Arc<dyn Provider>,
        registry: Arc<ToolRegistry>,
        ctx: Arc<ToolCtx>,
        config: AgentConfig,
        rollout: Rollout,
    ) -> Self {
        Self::with_source(
            provider,
            registry,
            ctx,
            config,
            SessionSource::Rollout(rollout),
        )
    }

    fn with_source(
        provider: Arc<dyn Provider>,
        registry: Arc<ToolRegistry>,
        ctx: Arc<ToolCtx>,
        config: AgentConfig,
        source: SessionSource,
    ) -> Self {
        Self {
            provider,
            registry,
            ctx,
            config,
            source,
            step_hooks: Vec::new(),
            observation_hooks: Vec::new(),
            reactive_sections: Vec::new(),
        }
    }

    /// Add a per-step request contributor.
    ///
    /// Contributors are installed in declaration order, so each sees the edits
    /// made by earlier contributors.
    pub fn step_hook(mut self, hook: Arc<dyn StepPrepareHook>) -> Self {
        self.step_hooks.push(hook);
        self
    }

    /// Add a passive tool-traffic observer.
    ///
    /// Observers are installed in declaration order and cannot mutate the
    /// traffic they receive.
    pub fn observation_hook(mut self, hook: Arc<dyn ObservationHook>) -> Self {
        self.observation_hooks.push(hook);
        self
    }

    /// Add a change-detected reactive context section.
    pub fn reactive_section(mut self, section: Arc<dyn ReactiveSection>) -> Self {
        self.reactive_sections.push(section);
        self
    }

    /// Construct the configured session.
    pub fn build(self) -> Session {
        let mut session = match self.source {
            SessionSource::Fresh => {
                Session::new(self.provider, self.registry, self.ctx, self.config)
            }
            SessionSource::Messages(history) => {
                Session::resume(self.provider, self.registry, self.ctx, self.config, history)
            }
            SessionSource::Rollout(rollout) => {
                Session::resume_from(self.provider, self.registry, self.ctx, self.config, rollout)
            }
        };

        for hook in self.step_hooks {
            session.add_step_hook(hook);
        }
        for hook in self.observation_hooks {
            session.add_observation_hook(hook);
        }
        for section in self.reactive_sections {
            session.add_reactive_section(section);
        }
        session
    }
}

/// One item produced by a [`TurnPump`].
///
/// Every [`Event`](Self::Event) precedes exactly one
/// [`Terminal`](Self::Terminal). A terminal result is yielded even when the
/// runtime returns an error.
#[derive(Debug)]
pub enum TurnPumpItem {
    Event(AgentEvent),
    Terminal(Result<StopReason, RuntimeError>),
}

type TurnFuture<'a> = Pin<Box<dyn Future<Output = Result<StopReason, RuntimeError>> + Send + 'a>>;

/// A borrow-based driver for one session turn.
///
/// The pump owns neither the session nor a task. Consume it through
/// [`next`](Self::next) until its terminal item. If a host triggers the
/// session's cancellation token but does not want to forward trailing events,
/// it must use [`drain_to_terminal`](Self::drain_to_terminal) before dropping
/// the pump so the runtime can record its cancellation boundary. Dropping an
/// in-progress pump is reserved for an abruptly disconnected consumer and
/// deliberately leaves the rollout's ragged edge for recovery to heal.
pub struct TurnPump<'a> {
    events: UnboundedReceiver<AgentEvent>,
    run: Option<TurnFuture<'a>>,
    terminal: Option<Result<StopReason, RuntimeError>>,
    finished: bool,
}

impl<'a> TurnPump<'a> {
    /// Start driving one turn while borrowing `session`.
    pub fn new(session: &'a mut Session, user_text: impl Into<String>) -> Self {
        let (sink, events) = unbounded_channel();
        let user_text = user_text.into();
        let run = Box::pin(session.run_turn(user_text, sink));
        Self {
            events,
            run: Some(run),
            terminal: None,
            finished: false,
        }
    }

    /// Yield the next ordered runtime event or the single terminal result.
    pub async fn next(&mut self) -> Option<TurnPumpItem> {
        loop {
            if self.finished {
                return None;
            }

            if self.terminal.is_some() {
                match self.events.try_recv() {
                    Ok(event) => return Some(TurnPumpItem::Event(event)),
                    Err(TryRecvError::Empty | TryRecvError::Disconnected) => {
                        self.finished = true;
                        return self.terminal.take().map(TurnPumpItem::Terminal);
                    }
                }
            }

            let run = self
                .run
                .as_mut()
                .expect("a non-terminal turn pump must hold its run future");
            tokio::select! {
                // Prefer already-buffered runtime events to the terminal future.
                // If the run finishes in the same poll, the terminal branch
                // stores its result and the loop drains the channel first.
                biased;
                Some(event) = self.events.recv() => {
                    return Some(TurnPumpItem::Event(event));
                }
                result = run.as_mut() => {
                    self.run = None;
                    self.terminal = Some(result);
                }
            }
        }
    }

    /// Discard remaining events while driving the turn to its terminal result.
    ///
    /// This does not initiate cancellation: the host first fires the
    /// cancellation token it installed in `ToolCtx`, then calls this method.
    /// The separation keeps cancellation authority host-owned while ensuring
    /// `Session::run_turn` gets to execute its normal cleanup.
    pub async fn drain_to_terminal(&mut self) -> Option<Result<StopReason, RuntimeError>> {
        while let Some(item) = self.next().await {
            if let TurnPumpItem::Terminal(result) = item {
                return Some(result);
            }
        }
        None
    }
}
