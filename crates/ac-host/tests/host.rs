use std::sync::{Arc, Mutex};

use ac_context::{Cadence, FragmentClass, ReactiveSection};
use ac_host::{AgentHostBuilder, TurnPump, TurnPumpItem};
use ac_provider::CompletionRequest;
use ac_provider_mock::{MockProvider, stop_end, stop_tool_use, text, tool_use};
use ac_rollout::Rollout;
use ac_runtime::{AgentConfig, AgentEvent, Observation, ObservationHook, StepPrepareHook};
use ac_tool::{Capability, SubtreePolicy, Tool, ToolCtx, ToolOutput, ToolRegistry};
use ac_types::{Message, Role, StopReason};
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize, schemars::JsonSchema)]
struct EchoInput {
    text: String,
}

struct Echo;

impl Tool for Echo {
    type Input = EchoInput;

    fn name(&self) -> &'static str {
        "echo"
    }

    fn description(&self) -> String {
        "Echo text".into()
    }

    fn capability(&self) -> Capability {
        Capability::ReadOnly
    }

    fn run(
        self: Arc<Self>,
        input: Self::Input,
        _ctx: Arc<ToolCtx>,
    ) -> futures::future::BoxFuture<'static, ToolOutput> {
        Box::pin(async move { ToolOutput::ok(input.text) })
    }
}

struct AppendModel(&'static str);

impl StepPrepareHook for AppendModel {
    fn prepare(&self, _iteration: usize, request: &mut CompletionRequest) {
        request.model.push_str(self.0);
    }
}

struct Recorder(Arc<Mutex<Vec<Observation>>>);

impl ObservationHook for Recorder {
    fn observe(&self, event: &Observation) {
        self.0.lock().unwrap().push(event.clone());
    }
}

struct StandingSection {
    class: FragmentClass,
}

impl ReactiveSection for StandingSection {
    fn class(&self) -> &FragmentClass {
        &self.class
    }

    fn body(&self) -> Option<String> {
        Some("standing host state".into())
    }
}

fn ctx() -> (Arc<ToolCtx>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let policy = SubtreePolicy::new(dir.path()).unwrap();
    (Arc::new(ToolCtx::new(Arc::new(policy))), dir)
}

fn registry() -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    registry.register(Echo);
    Arc::new(registry)
}

#[tokio::test]
async fn builder_composes_contributors_and_pump_orders_terminal_last() {
    let provider = MockProvider::new(vec![
        vec![
            tool_use("call-1", "echo", json!({ "text": "hello" })),
            stop_tool_use(),
        ],
        vec![text("done"), stop_end()],
    ]);
    let provider_handle = provider.clone();
    let (ctx, _dir) = ctx();
    let observations = Arc::new(Mutex::new(Vec::new()));
    let section = StandingSection {
        class: FragmentClass::new(
            "host-state",
            Role::User,
            "[[host-state]]",
            "[[/host-state]]",
            Some(Cadence::Reactive),
            1024,
        ),
    };

    let mut session = AgentHostBuilder::new(
        Arc::new(provider),
        registry(),
        ctx,
        AgentConfig {
            model: "model".into(),
            ..Default::default()
        },
    )
    .step_hook(Arc::new(AppendModel("-first")))
    .step_hook(Arc::new(AppendModel("-second")))
    .observation_hook(Arc::new(Recorder(observations.clone())))
    .reactive_section(Arc::new(section))
    .build();

    let mut pump = TurnPump::new(&mut session, "go");
    let mut items = Vec::new();
    while let Some(item) = pump.next().await {
        items.push(item);
    }

    let order: Vec<&str> = items
        .iter()
        .map(|item| match item {
            TurnPumpItem::Event(AgentEvent::ToolCall { .. }) => "tool_call",
            TurnPumpItem::Event(AgentEvent::ToolResult { .. }) => "tool_result",
            TurnPumpItem::Event(AgentEvent::Text(_)) => "text",
            TurnPumpItem::Event(AgentEvent::TurnComplete { .. }) => "turn_complete",
            TurnPumpItem::Event(_) => "other_event",
            TurnPumpItem::Terminal(_) => "terminal",
        })
        .collect();
    assert_eq!(
        order,
        vec![
            "tool_call",
            "tool_result",
            "text",
            "turn_complete",
            "terminal"
        ]
    );
    assert!(matches!(
        items.last(),
        Some(TurnPumpItem::Terminal(Ok(StopReason::EndTurn)))
    ));
    assert_eq!(
        items
            .iter()
            .filter(|item| matches!(item, TurnPumpItem::Terminal(_)))
            .count(),
        1
    );
    let event_count = items
        .iter()
        .filter(|item| matches!(item, TurnPumpItem::Event(_)))
        .count();
    assert_eq!(event_count + 1, items.len());
    assert!(items.iter().any(|item| matches!(
        item,
        TurnPumpItem::Event(AgentEvent::TurnComplete {
            stop_reason: StopReason::EndTurn
        })
    )));

    let requests = provider_handle.requests();
    assert_eq!(requests[0].model, "model-first-second");
    assert!(requests[0].messages.iter().any(|message| {
        message.content.iter().any(
            |part| matches!(part, ac_types::ContentPart::Text { text } if text.contains("standing host state")),
        )
    }));
    assert_eq!(
        *observations.lock().unwrap(),
        vec![
            Observation::ToolStart {
                id: "call-1".into(),
                name: "echo".into(),
            },
            Observation::ToolFinish {
                id: "call-1".into(),
                name: "echo".into(),
                is_error: false,
            },
        ]
    );
}

#[tokio::test]
async fn pump_yields_a_runtime_error_as_its_only_terminal_item() {
    let provider = Arc::new(MockProvider::new(vec![]));
    let (ctx, _dir) = ctx();
    let mut session = AgentHostBuilder::new(
        provider,
        registry(),
        ctx,
        AgentConfig {
            max_iterations: 0,
            ..Default::default()
        },
    )
    .build();

    let mut pump = TurnPump::new(&mut session, "go");
    assert!(matches!(
        pump.next().await,
        Some(TurnPumpItem::Terminal(Err(
            ac_runtime::RuntimeError::MaxIterations(0)
        )))
    ));
    assert!(pump.next().await.is_none());
}

#[tokio::test]
async fn cancelled_turn_can_be_drained_to_a_closed_rollout_boundary() {
    let provider = Arc::new(MockProvider::new(vec![]));
    let (ctx, _dir) = ctx();
    let cancel = ctx.cancel.clone();
    let mut session =
        AgentHostBuilder::new(provider, registry(), ctx, AgentConfig::default()).build();

    let mut pump = TurnPump::new(&mut session, "go");
    cancel.cancel();
    assert!(matches!(
        pump.drain_to_terminal().await,
        Some(Err(ac_runtime::RuntimeError::Cancelled))
    ));
    assert!(pump.next().await.is_none());
    drop(pump);
    assert_eq!(session.rollout().open_turn(), None);
}

#[test]
fn builder_supports_flat_and_rollout_resume_without_owning_storage() {
    let provider = Arc::new(MockProvider::new(vec![]));
    let (ctx, _dir) = ctx();
    let history = vec![Message::text(Role::User, "flat history")];
    let flat = AgentHostBuilder::resume(
        provider.clone(),
        registry(),
        ctx.clone(),
        AgentConfig::default(),
        history,
    )
    .build();
    assert!(flat.messages().iter().any(|message| {
        message.content.iter().any(
            |part| matches!(part, ac_types::ContentPart::Text { text } if text == "flat history"),
        )
    }));

    let mut rollout = Rollout::create();
    rollout.record_message(Message::text(Role::User, "rollout history"));
    let resumed =
        AgentHostBuilder::resume_from(provider, registry(), ctx, AgentConfig::default(), rollout)
            .build();
    assert!(resumed.messages().iter().any(|message| {
        message.content.iter().any(
            |part| matches!(part, ac_types::ContentPart::Text { text } if text == "rollout history"),
        )
    }));
}
