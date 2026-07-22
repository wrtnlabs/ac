//! The kit's reference sub-agent spawner ([docs/ac-subagents.md]).
//!
//! [`AgentSpawner`] is the seam ([ac-tool]); this is the kit's reference
//! *mechanism* behind it, the same way `ac-sandbox` is the mechanism behind the
//! sandbox launcher. It owns the fiddly, reusable **run mechanics** — deriving
//! the child's cancellation token, driving the child's turn against a *consumed*
//! sink (a dropped sink would read as an implicit cancel — §5), extracting the
//! child's final text, and mapping the loop's termination onto a
//! [`SpawnStatus`]. The **policy-laden assembly** — the child's provider,
//! its tool surface filtered per the agent definition and never carrying the
//! delegation tool, its config, and its narrowed containment — stays the host's,
//! supplied as an assembler closure. The child a host assembles MUST have no
//! spawner installed (the structural recursion guard) and containment no wider
//! than the parent's.

use ac_tool::{AgentSpawner, SpawnRequest, SpawnResult, SpawnStatus};
use ac_types::{ContentPart, Message, Role};
use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;

use crate::{AgentEvent, RuntimeError, Session};

/// A reference [`AgentSpawner`] over a host-supplied child-assembly closure
/// `assemble(&request, child_cancel) -> Option<Session>` (`None` for an unknown
/// agent). The assembler builds the child `Session` — resolving `request.agent`,
/// applying any `request.model` override, installing the given cancellation
/// token, and leaving the spawner slot empty — and this type runs it.
pub struct ReferenceSpawner<F> {
    assemble: F,
}

impl<F> ReferenceSpawner<F>
where
    F: Fn(&SpawnRequest, CancellationToken) -> Option<Session> + Send + Sync + 'static,
{
    pub fn new(assemble: F) -> Self {
        Self { assemble }
    }
}

impl<F> AgentSpawner for ReferenceSpawner<F>
where
    F: Fn(&SpawnRequest, CancellationToken) -> Option<Session> + Send + Sync + 'static,
{
    fn spawn(&self, req: SpawnRequest) -> BoxFuture<'static, SpawnResult> {
        // Derive the child's token from the parent's (cancel flows down, not up),
        // then assemble synchronously before entering the future. The assembler
        // sees the whole request, so a per-child model override is honored.
        let child_cancel = req.cancel.child_token();
        let assembled = (self.assemble)(&req, child_cancel);
        let SpawnRequest { agent, prompt, .. } = req;
        Box::pin(async move {
            let Some(mut session) = assembled else {
                return SpawnResult {
                    session_id: String::new(),
                    output: String::new(),
                    status: SpawnStatus::Error(format!("unknown agent: {agent}")),
                };
            };

            // The child's sink MUST be consumed — a dropped sink is read as an
            // implicit cancel at the next step boundary ([ac-loop] §5). We route
            // it to a drain (a host that wants the child's stream pumps it here).
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
            let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });

            let status = match session.run_turn(prompt, tx).await {
                Ok(_) => SpawnStatus::Completed,
                Err(RuntimeError::Cancelled)
                | Err(RuntimeError::MaxIterations(_))
                | Err(RuntimeError::Timeout) => SpawnStatus::Aborted,
                Err(e) => SpawnStatus::Error(e.to_string()),
            };
            let _ = drain.await;

            SpawnResult {
                session_id: session.rollout().id().to_string(),
                output: final_text(&session.messages()),
                status,
            }
        })
    }
}

/// The child's **final text**: the concatenated text of its terminal assistant
/// message (the last assistant message in the projected log), not a fold of
/// every assistant utterance. Empty when the child produced none.
fn final_text(messages: &[Message]) -> String {
    messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)
        .map(|m| {
            m.content
                .iter()
                .filter_map(|p| match p {
                    ContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<String>()
        })
        .unwrap_or_default()
}
