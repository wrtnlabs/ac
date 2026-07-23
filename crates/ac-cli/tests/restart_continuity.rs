//! Restart continuity at the library layer ([docs/ac-durability.md] §6, 5.1):
//! the "close the machine, open it tomorrow" contract, driven over the shipped
//! host wiring with a scripted `MockProvider` and a real `SqliteStore` in a
//! tempdir. Phase 1 runs a tool-using turn and persists it input-first
//! (§3.1); phase 2 drops every in-memory artifact; phase 3 reopens the store
//! fresh and resumes from what it holds alone. The mid-turn variant abandons
//! a turn inside the 5.1 crash window and proves only the in-flight output is
//! lost — never the input that provoked it.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ac_provider::{CompletionRequest, EventStream, Provider};
use ac_provider_mock::{MockProvider, stop_end, stop_tool_use, text, tool_use};
use ac_runtime::{AgentConfig, AgentEvent, Session};
use ac_store::SqliteStore;
use ac_tool::{PathPolicy, SubtreePolicy, ToolCtx};
use ac_types::{CompletionError, ContentPart, Message, Role, StopReason};
use futures::future::BoxFuture;
use serde_json::json;

/// All text parts of a message, concatenated.
fn text_of(m: &Message) -> String {
    m.content
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// Drive one turn to completion, handing the session back for persistence.
async fn run(
    mut session: Session,
    prompt: &str,
) -> (Result<StopReason, String>, Vec<AgentEvent>, Session) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    let prompt = prompt.to_string();
    let driver = tokio::spawn(async move {
        let result = session.run_turn(prompt, tx).await;
        (result, session)
    });
    let mut events = Vec::new();
    while let Some(ev) = rx.recv().await {
        events.push(ev);
    }
    let (result, session) = driver.await.expect("join");
    (result.map_err(|e| e.to_string()), events, session)
}

/// Rebuild a session from a loaded flat history — the reopen path a persisting
/// host takes ([docs/ac-durability.md] §3.2: from the store, not from memory).
fn resumed_session(
    provider: MockProvider,
    dir: &std::path::Path,
    history: Vec<Message>,
) -> Session {
    let policy: Arc<dyn PathPolicy> = Arc::new(SubtreePolicy::new(dir).expect("policy"));
    let config = AgentConfig {
        model: "mock/model".to_string(),
        system: Some(ac_cli::SYSTEM_PROMPT.to_string()),
        max_iterations: ac_cli::MAX_ITERATIONS,
        ..Default::default()
    };
    Session::resume(
        Arc::new(provider),
        Arc::new(ac_cli::generic_registry()),
        Arc::new(ToolCtx::new(policy)),
        config,
        history,
    )
}

#[tokio::test]
async fn restart_continuity_survives_close_and_reopen() {
    let ws = tempfile::tempdir().unwrap();
    std::fs::write(ws.path().join("notes.txt"), "alpha-seed").unwrap();
    let store_dir = tempfile::tempdir().unwrap();
    let db = store_dir.path().join("sessions.db");

    // --- phase 1: the day ---
    let store = SqliteStore::open(&db).unwrap();
    let rec = store.create_session(Some("continuity")).unwrap();
    let prompt = "read notes.txt and report what it says";
    // Input-first (§3.1): the user's message reaches the store before the
    // turn's first sample.
    let next = store
        .append_messages(&rec.id, &[Message::text(Role::User, prompt)], Some(0))
        .unwrap();
    assert_eq!(next, 1);

    let provider = MockProvider::new(vec![
        vec![
            tool_use("call-read", "read_file", json!({ "path": "notes.txt" })),
            stop_tool_use(),
        ],
        vec![text("the notes say alpha-seed"), stop_end()],
    ]);
    let host = ac_cli::build_host(
        Arc::new(provider),
        ws.path(),
        "mock/model".to_string(),
        ac_cli::HostOptions::default(),
    )
    .expect("build_host");
    let ac_cli::GenericHost { session, ctx, .. } = host;

    let (result, _events, session) = run(session, prompt).await;
    assert_eq!(result.expect("turn ok"), StopReason::EndTurn);

    // Output at settle: everything the session gained past the persisted input.
    let msgs = session.messages();
    assert_eq!(text_of(&msgs[0]), prompt);
    let appended = store.append_messages(&rec.id, &msgs[1..], Some(1)).unwrap();
    assert_eq!(appended as usize, msgs.len());

    // --- phase 2: close — every in-memory artifact dies ---
    drop(session);
    drop(ctx);
    drop(store);

    // --- phase 3: next day — reopen fresh and resume from the store alone ---
    let store = SqliteStore::open(&db).unwrap();
    let history = store.load_messages(&rec.id).unwrap();
    assert_eq!(
        history.len(),
        4,
        "input + assistant tool call + tool result + answer"
    );

    let follow_up = "what did the notes say, again?";
    let next = store
        .append_messages(&rec.id, &[Message::text(Role::User, follow_up)], Some(4))
        .unwrap();
    assert_eq!(next, 5);

    let provider = MockProvider::new(vec![vec![text("alpha-seed, as before"), stop_end()]]);
    let session = resumed_session(provider.clone(), ws.path(), history);
    let (result, _events, session) = run(session, follow_up).await;
    assert_eq!(result.expect("resumed turn ok"), StopReason::EndTurn);

    // The follow-up request carried turn 1's content back to the model —
    // continuity is real, not a fresh context.
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    let seen = &requests[0].messages;
    assert_eq!(seen.len(), 5, "4 restored messages + the follow-up");
    assert!(
        seen.iter()
            .any(|m| m.role == Role::Assistant && text_of(m).contains("alpha-seed")),
        "turn 1's assistant text must be visible to the resumed turn"
    );
    assert!(
        seen.iter().any(|m| m.content.iter().any(
            |p| matches!(p, ContentPart::ToolResult(tr) if tr.content.contains("alpha-seed"))
        )),
        "turn 1's tool result must survive the round-trip through the store"
    );

    // Persist turn 2's output; the store now holds both turns, seq contiguous
    // from 0 (every CAS above passed).
    let msgs = session.messages();
    let appended = store.append_messages(&rec.id, &msgs[5..], Some(5)).unwrap();
    assert_eq!(appended, 6);
    assert_eq!(store.message_count(&rec.id).unwrap(), 6);
    let all = store.load_messages(&rec.id).unwrap();
    let roles: Vec<Role> = all.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        vec![
            Role::User,
            Role::Assistant,
            Role::User,
            Role::Assistant,
            Role::User,
            Role::Assistant,
        ]
    );
    assert!(text_of(&all[5]).contains("alpha-seed, as before"));
}

/// Delegates its first `stall_from` calls to the scripted mock, then stalls
/// mid-stream forever — pinning the turn inside the 5.1 crash window so the
/// test can kill it deterministically after the first step.
struct StallAfter {
    inner: MockProvider,
    stall_from: usize,
    calls: AtomicUsize,
}

impl Provider for StallAfter {
    fn name(&self) -> &str {
        "stall-after"
    }

    fn stream_completion(
        &self,
        request: CompletionRequest,
    ) -> BoxFuture<'static, Result<EventStream, CompletionError>> {
        if self.calls.fetch_add(1, Ordering::SeqCst) >= self.stall_from {
            return Box::pin(async { Ok(Box::pin(futures::stream::pending()) as EventStream) });
        }
        self.inner.stream_completion(request)
    }
}

#[tokio::test]
async fn restart_mid_turn_loses_only_inflight_output() {
    let ws = tempfile::tempdir().unwrap();
    std::fs::write(ws.path().join("notes.txt"), "alpha-seed").unwrap();
    let store_dir = tempfile::tempdir().unwrap();
    let db = store_dir.path().join("sessions.db");

    let store = SqliteStore::open(&db).unwrap();
    let rec = store.create_session(Some("mid-turn")).unwrap();
    let prompt = "read notes.txt and summarize";
    // Input-first (§3.1) — the only append this turn will ever get.
    store
        .append_messages(&rec.id, &[Message::text(Role::User, prompt)], Some(0))
        .unwrap();

    let provider = StallAfter {
        inner: MockProvider::new(vec![vec![
            tool_use("call-read", "read_file", json!({ "path": "notes.txt" })),
            stop_tool_use(),
        ]]),
        stall_from: 1,
        calls: AtomicUsize::new(0),
    };
    let host = ac_cli::build_host(
        Arc::new(provider),
        ws.path(),
        "mock/model".to_string(),
        ac_cli::HostOptions::default(),
    )
    .expect("build_host");
    let ac_cli::GenericHost {
        mut session, ctx, ..
    } = host;

    // Run until the first step settles (its tool result arrives), then abort
    // the driver on the stalled second request — process death, not a graceful
    // cancel: no marker, no settle append, the session simply ceases.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    let owned_prompt = prompt.to_string();
    let driver = tokio::spawn(async move { session.run_turn(owned_prompt, tx).await });
    loop {
        match rx.recv().await {
            Some(AgentEvent::ToolResult { .. }) => break,
            Some(_) => continue,
            None => panic!("the turn ended before its first tool result"),
        }
    }
    driver.abort();
    assert!(driver.await.unwrap_err().is_cancelled());
    drop(ctx);
    drop(store);

    // --- reopen: exactly the input survived, zero assistant garbage (5.1) ---
    let store = SqliteStore::open(&db).unwrap();
    let history = store.load_messages(&rec.id).unwrap();
    assert_eq!(history.len(), 1, "only the input-first user message");
    assert_eq!(history[0].role, Role::User);
    assert_eq!(text_of(&history[0]), prompt);

    // --- and the session works again: a fresh turn on the resumed history ---
    let follow_up = "let's continue";
    store
        .append_messages(&rec.id, &[Message::text(Role::User, follow_up)], Some(1))
        .unwrap();
    let provider = MockProvider::new(vec![vec![text("back on track"), stop_end()]]);
    let session = resumed_session(provider.clone(), ws.path(), history);
    let (result, _events, session) = run(session, follow_up).await;
    assert_eq!(result.expect("fresh turn ok"), StopReason::EndTurn);

    // The fresh turn saw the surviving input and nothing torn.
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].messages.len(), 2, "survivor input + follow-up");
    assert!(
        requests[0].messages.iter().all(|m| m.role == Role::User),
        "no partial assistant output leaked into the resumed context"
    );

    let msgs = session.messages();
    let appended = store.append_messages(&rec.id, &msgs[2..], Some(2)).unwrap();
    assert_eq!(appended, 3);
    let all = store.load_messages(&rec.id).unwrap();
    let roles: Vec<Role> = all.iter().map(|m| m.role).collect();
    assert_eq!(roles, vec![Role::User, Role::User, Role::Assistant]);
    assert!(text_of(&all[2]).contains("back on track"));
}
