//! The reactive-section driver through the real loop ([docs/ac-ultra.md] §4): a
//! delegation-mode-style section injects at session start, stays silent across
//! turns while unchanged (idempotent, the meaningful-silence property), and
//! emits a superseding fragment when the host flips it. Hermetic — MockProvider,
//! no network.

use std::sync::{Arc, Mutex};

use ac_context::{Cadence, FragmentClass, ReactiveSection};
use ac_provider_mock::{MockProvider, stop_end, text};
use ac_runtime::{AgentConfig, AgentEvent, Session};
use ac_tool::{SubtreePolicy, ToolCtx, ToolRegistry};

/// A mode section backed by a flip handle — the shape of ac-cli's delegation
/// mode, minus the prose.
struct ModeSection {
    class: FragmentClass,
    mode: Arc<Mutex<&'static str>>,
}

impl ReactiveSection for ModeSection {
    fn class(&self) -> &FragmentClass {
        &self.class
    }
    fn body(&self) -> Option<String> {
        Some(format!("delegation mode is {}", *self.mode.lock().unwrap()))
    }
}

fn mode_class() -> FragmentClass {
    FragmentClass::new(
        "delegation-mode",
        ac_types::Role::User,
        "[[delegation-mode]]",
        "[[/delegation-mode]]",
        Some(Cadence::Reactive),
        4096,
    )
}

async fn run(session: &mut Session, prompt: &str) {
    // Keep the receiver alive for the turn (a dropped sink is an implicit
    // cancel); the unbounded channel buffers, so no concurrent drain is needed.
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    session
        .run_turn(prompt.to_string(), tx)
        .await
        .expect("turn ok");
}

fn mode_count(req: &ac_provider::CompletionRequest, needle: &str) -> usize {
    req.messages
        .iter()
        .filter(|m| {
            m.content
                .iter()
                .any(|p| matches!(p, ac_types::ContentPart::Text { text } if text.contains(needle)))
        })
        .count()
}

#[tokio::test]
async fn a_mode_injects_once_stays_silent_then_supersedes_on_flip() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        vec![text("t1"), stop_end()],
        vec![text("t2"), stop_end()],
        vec![text("t3"), stop_end()],
    ]);
    let handle = provider.clone();
    let ctx = Arc::new(ToolCtx::new(Arc::new(
        SubtreePolicy::new(dir.path()).unwrap(),
    )));
    let mut session = Session::new(
        Arc::new(provider),
        Arc::new(ToolRegistry::new()),
        ctx,
        AgentConfig::default(),
    );

    let mode = Arc::new(Mutex::new("proactive"));
    session.add_reactive_section(Arc::new(ModeSection {
        class: mode_class(),
        mode: mode.clone(),
    }));

    // Turn 1: session start injects the current mode.
    run(&mut session, "one").await;
    // Turn 2: unchanged → the driver stays silent (no re-injection).
    run(&mut session, "two").await;
    // Flip, then turn 3: a superseding fragment is emitted.
    *mode.lock().unwrap() = "on-request";
    run(&mut session, "three").await;

    let reqs = handle.requests();
    // Turn 1's request carried the proactive mode (session-start injection).
    assert_eq!(mode_count(&reqs[0], "delegation mode is proactive"), 1);
    // Turn 2's request still has EXACTLY ONE proactive fragment — the driver did
    // not re-inject an unchanged mode (idempotent silence).
    assert_eq!(
        mode_count(&reqs[1], "delegation mode is proactive"),
        1,
        "an unchanged mode must not be re-injected"
    );
    // Turn 3's request carries the flipped mode (a superseding fragment).
    assert_eq!(mode_count(&reqs[2], "delegation mode is on-request"), 1);
}
