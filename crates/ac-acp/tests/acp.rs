//! Hermetic ACP tests: a real ACP client drives the AC agent in-process (the
//! agent builder doubles as the client's transport — real dispatch, real
//! schema types, no sockets). The provider is scripted; the tools are real.

use std::path::Path;
use std::sync::Arc;

use ac_acp::{AcpOptions, SessionParts};
use ac_provider_mock::{MockProvider, stop_end, stop_tool_use, text, tool_use};
use ac_runtime::AgentConfig;
use ac_store::SqliteStore;
use ac_tool::{Capability, SubtreePolicy, Tool, ToolCtx, ToolOutput, ToolRegistry};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    CancelNotification, ContentBlock, ContentChunk, InitializeRequest, LoadSessionRequest,
    NewSessionRequest, PromptRequest, SessionId, SessionNotification, SessionUpdate, StopReason,
    ToolCallStatus,
};
use agent_client_protocol::{Client, ConnectionTo};
use tokio::sync::mpsc;

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct SleepyInput {}

/// Sleeps until cancelled — holds a turn open so a test can cancel it.
struct Sleepy;

impl Tool for Sleepy {
    type Input = SleepyInput;
    fn name(&self) -> &'static str {
        "sleepy"
    }
    fn description(&self) -> String {
        "sleeps for a long time".into()
    }
    fn capability(&self) -> Capability {
        Capability::ReadOnly
    }
    fn run(
        self: Arc<Self>,
        _input: Self::Input,
        ctx: Arc<ToolCtx>,
    ) -> futures::future::BoxFuture<'static, ToolOutput> {
        Box::pin(async move {
            tokio::select! {
                _ = ctx.cancel.cancelled() => ToolOutput::error("sleepy cancelled"),
                _ = tokio::time::sleep(std::time::Duration::from_secs(300)) => {
                    ToolOutput::ok("slept")
                }
            }
        })
    }
}

fn options_for(provider: MockProvider, store: Option<Arc<SqliteStore>>) -> AcpOptions {
    let factory = Arc::new(move |cwd: &Path| {
        let policy = SubtreePolicy::new(cwd).map_err(|e| e.to_string())?;
        let mut registry = ToolRegistry::new();
        registry.register(Sleepy);
        Ok(SessionParts {
            provider: Arc::new(provider.clone()),
            registry: Arc::new(registry),
            config: AgentConfig::default(),
            ctx: Arc::new(ToolCtx::new(Arc::new(policy))),
        })
    });
    let mut options = AcpOptions::new(factory);
    options.store = store;
    options
}

fn chunk_text(chunk: &ContentChunk) -> String {
    match &chunk.content {
        ContentBlock::Text(t) => t.text.clone(),
        _ => String::new(),
    }
}

fn drain(rx: &mut mpsc::UnboundedReceiver<SessionNotification>) -> Vec<SessionUpdate> {
    let mut out = Vec::new();
    while let Ok(n) = rx.try_recv() {
        out.push(n.update);
    }
    out
}

#[tokio::test]
async fn prompt_streams_the_full_tool_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    // unknown_tool dispatches through the registry → error data → the model
    // recovers next iteration; the update stream must show all of it.
    let provider = MockProvider::new(vec![
        vec![
            tool_use("c1", "unknown_tool", serde_json::json!({})),
            stop_tool_use(),
        ],
        vec![text("all done"), stop_end()],
    ]);
    let agent = ac_acp::agent(options_for(provider.clone(), None));
    let (tx, mut updates) = mpsc::unbounded_channel::<SessionNotification>();
    let dir_path = dir.path().to_path_buf();

    Client
        .builder()
        .name("test-client")
        .on_receive_notification(
            async move |n: SessionNotification, _cx| {
                let _ = tx.send(n);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(agent, async move |cx: ConnectionTo<_>| {
            let init = cx
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            assert!(!init.agent_capabilities.load_session);

            let session = cx
                .send_request(NewSessionRequest::new(dir_path))
                .block_task()
                .await?;
            let response = cx
                .send_request(PromptRequest::new(
                    session.session_id.clone(),
                    vec![ContentBlock::from("go")],
                ))
                .block_task()
                .await?;
            assert_eq!(response.stop_reason, StopReason::EndTurn);
            Ok(())
        })
        .await
        .unwrap();

    let updates = drain(&mut updates);
    assert!(
        matches!(&updates[0], SessionUpdate::ToolCall(tc) if tc.tool_call_id.0.as_ref() == "c1")
    );
    assert!(updates.iter().any(|u| matches!(
        u,
        SessionUpdate::ToolCallUpdate(tu)
            if tu.tool_call_id.0.as_ref() == "c1"
            && tu.fields.status == Some(ToolCallStatus::Failed)
    )));
    assert!(
        updates.iter().any(
            |u| matches!(u, SessionUpdate::AgentMessageChunk(c) if chunk_text(c) == "all done")
        )
    );
    assert_eq!(provider.call_count(), 2);
}

#[tokio::test]
async fn cancel_mid_turn_then_session_still_works() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        vec![
            tool_use("c1", "sleepy", serde_json::json!({})),
            stop_tool_use(),
        ],
        // After the cancelled turn the session is rebuilt; the next prompt
        // pops this script.
        vec![text("recovered"), stop_end()],
    ]);
    let agent = ac_acp::agent(options_for(provider.clone(), None));
    let (tx, mut updates) = mpsc::unbounded_channel::<SessionNotification>();
    let dir_path = dir.path().to_path_buf();

    Client
        .builder()
        .name("test-client")
        .on_receive_notification(
            async move |n: SessionNotification, _cx| {
                let _ = tx.send(n);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(agent, async move |cx: ConnectionTo<_>| {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let session = cx
                .send_request(NewSessionRequest::new(dir_path))
                .block_task()
                .await?;
            let sid = session.session_id.clone();

            let pending = cx.send_request(PromptRequest::new(
                sid.clone(),
                vec![ContentBlock::from("nap time")],
            ));

            // Wait until the tool call is actually in flight, then cancel.
            loop {
                let n = updates.recv().await.expect("update stream open");
                if matches!(n.update, SessionUpdate::ToolCall(_)) {
                    break;
                }
            }
            cx.send_notification(CancelNotification::new(sid.clone()))?;

            let response = pending.block_task().await?;
            assert_eq!(response.stop_reason, StopReason::Cancelled);

            // The session survives its own cancellation: a fresh prompt runs.
            let response = cx
                .send_request(PromptRequest::new(
                    sid,
                    vec![ContentBlock::from("you back?")],
                ))
                .block_task()
                .await?;
            assert_eq!(response.stop_reason, StopReason::EndTurn);
            Ok(())
        })
        .await
        .unwrap();
}

/// The stop-button-vs-turn-completion race: a cancel that lands with no turn
/// in flight must be a no-op, not poison the next prompt into a spurious
/// Cancelled.
#[tokio::test]
async fn idle_cancel_does_not_poison_the_next_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![
        vec![text("one"), stop_end()],
        vec![text("two"), stop_end()],
    ]);
    let agent = ac_acp::agent(options_for(provider.clone(), None));
    let dir_path = dir.path().to_path_buf();

    Client
        .builder()
        .name("test-client")
        .connect_with(agent, async move |cx: ConnectionTo<_>| {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let session = cx
                .send_request(NewSessionRequest::new(dir_path))
                .block_task()
                .await?;
            let sid = session.session_id.clone();

            let response = cx
                .send_request(PromptRequest::new(
                    sid.clone(),
                    vec![ContentBlock::from("first")],
                ))
                .block_task()
                .await?;
            assert_eq!(response.stop_reason, StopReason::EndTurn);

            // Cancel arrives after the turn already completed.
            cx.send_notification(CancelNotification::new(sid.clone()))?;

            let response = cx
                .send_request(PromptRequest::new(sid, vec![ContentBlock::from("second")]))
                .block_task()
                .await?;
            assert_eq!(
                response.stop_reason,
                StopReason::EndTurn,
                "an idle cancel must not turn the next prompt into Cancelled"
            );
            Ok(())
        })
        .await
        .unwrap();

    // Both prompts actually reached the provider.
    assert_eq!(provider.call_count(), 2);
}

#[tokio::test]
async fn load_replays_history_and_continues_with_context() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("acp.db");
    let cwd = dir.path().to_path_buf();

    // ---- connection one: create + one turn, persisted ---------------------
    let captured = Arc::new(std::sync::Mutex::new(String::new()));
    {
        let store = Arc::new(SqliteStore::open(&db).unwrap());
        let provider = MockProvider::new(vec![vec![text("cats are great"), stop_end()]]);
        let agent = ac_acp::agent(options_for(provider, Some(store)));
        let cwd = cwd.clone();
        let captured_in = captured.clone();

        Client
            .builder()
            .name("test-client")
            .connect_with(agent, async move |cx: ConnectionTo<_>| {
                let init = cx
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                assert!(init.agent_capabilities.load_session);
                let session = cx
                    .send_request(NewSessionRequest::new(cwd))
                    .block_task()
                    .await?;
                cx.send_request(PromptRequest::new(
                    session.session_id.clone(),
                    vec![ContentBlock::from("tell me about cats")],
                ))
                .block_task()
                .await?;
                *captured_in.lock().unwrap() = session.session_id.0.to_string();
                Ok(())
            })
            .await
            .unwrap();
    }
    let session_id = captured.lock().unwrap().clone();
    assert!(!session_id.is_empty());

    // ---- connection two: a fresh agent over the same store ----------------
    let store = Arc::new(SqliteStore::open(&db).unwrap());
    let provider = MockProvider::new(vec![vec![text("dogs too"), stop_end()]]);
    let agent = ac_acp::agent(options_for(provider.clone(), Some(store)));
    let (tx, mut updates) = mpsc::unbounded_channel::<SessionNotification>();
    let sid_for_load = session_id.clone();

    Client
        .builder()
        .name("test-client")
        .on_receive_notification(
            async move |n: SessionNotification, _cx| {
                let _ = tx.send(n);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(agent, async move |cx: ConnectionTo<_>| {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            cx.send_request(LoadSessionRequest::new(
                SessionId::new(sid_for_load.clone()),
                cwd.clone(),
            ))
            .block_task()
            .await?;

            let response = cx
                .send_request(PromptRequest::new(
                    SessionId::new(sid_for_load),
                    vec![ContentBlock::from("and dogs?")],
                ))
                .block_task()
                .await?;
            assert_eq!(response.stop_reason, StopReason::EndTurn);
            Ok(())
        })
        .await
        .unwrap();

    // The load replayed turn one as ordinary updates…
    let updates = drain(&mut updates);
    assert!(updates.iter().any(
        |u| matches!(u, SessionUpdate::UserMessageChunk(c) if chunk_text(c) == "tell me about cats")
    ));
    assert!(updates.iter().any(
        |u| matches!(u, SessionUpdate::AgentMessageChunk(c) if chunk_text(c) == "cats are great")
    ));

    // …and the continued turn carried the full history to the provider.
    let request = &provider.requests()[0];
    assert_eq!(request.messages.len(), 3);
}
