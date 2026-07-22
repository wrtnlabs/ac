//! The reload-recovery contract, end-to-end: a turn runs, its history is
//! persisted to a file-backed store, everything is dropped ("the process
//! exits"), then the store is reopened and the session resumed — and the
//! next turn's provider request must carry the full prior history.

use std::sync::Arc;

use ac_provider_mock::{MockProvider, stop_end, text};
use ac_runtime::{AgentConfig, Session};
use ac_store::SqliteStore;
use ac_tool::{SubtreePolicy, ToolCtx, ToolRegistry};
use ac_types::{ContentPart, Role, StopReason};
use tokio::sync::mpsc;

fn make_ctx(dir: &std::path::Path) -> Arc<ToolCtx> {
    let policy = SubtreePolicy::new(dir).unwrap();
    Arc::new(ToolCtx::new(Arc::new(policy)))
}

#[tokio::test]
async fn a_session_survives_a_process_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("store").join("ac.db");

    // ---- "process one": run a turn, persist, exit -------------------------
    let session_id = {
        let store = SqliteStore::open(&db_path).unwrap();
        let record = store.create_session(Some("cats")).unwrap();

        let provider = MockProvider::new(vec![vec![text("cats are great"), stop_end()]]);
        let mut session = Session::new(
            Arc::new(provider),
            Arc::new(ToolRegistry::new()),
            make_ctx(dir.path()),
            AgentConfig::default(),
        );
        let (tx, _rx) = mpsc::unbounded_channel();
        let stop = session
            .run_turn("tell me about cats".into(), tx)
            .await
            .unwrap();
        assert!(matches!(stop, StopReason::EndTurn));

        store
            .append_messages(&record.id, &session.messages(), None)
            .unwrap();
        record.id
        // store, session, provider all drop here — the "process" is gone.
    };

    // ---- "process two": reopen, resume, continue --------------------------
    let store = SqliteStore::open(&db_path).unwrap();
    let record = store.get_session(&session_id).unwrap().expect("persisted");
    assert_eq!(record.title.as_deref(), Some("cats"));
    let history = store.load_messages(&session_id).unwrap();
    assert_eq!(history.len(), 2, "user + assistant from turn one");

    let provider = MockProvider::new(vec![vec![text("dogs are great too"), stop_end()]]);
    let mut session = Session::resume(
        Arc::new(provider.clone()),
        Arc::new(ToolRegistry::new()),
        make_ctx(dir.path()),
        AgentConfig::default(),
        history,
    );
    let (tx, _rx) = mpsc::unbounded_channel();
    let stop = session.run_turn("and dogs?".into(), tx).await.unwrap();
    assert!(matches!(stop, StopReason::EndTurn));

    // The request the provider saw in turn two must carry turn one verbatim —
    // this is the assertion that rehydration actually restores context.
    let request = &provider.requests()[0];
    assert_eq!(request.messages.len(), 3);
    assert_eq!(request.messages[0].role, Role::User);
    assert!(
        matches!(&request.messages[0].content[0], ContentPart::Text { text } if text == "tell me about cats")
    );
    assert_eq!(request.messages[1].role, Role::Assistant);
    assert!(
        matches!(&request.messages[1].content[0], ContentPart::Text { text } if text == "cats are great")
    );
    assert!(
        matches!(&request.messages[2].content[0], ContentPart::Text { text } if text == "and dogs?")
    );

    // Persist only the delta from turn two: everything past what's stored.
    let stored = store.message_count(&session_id).unwrap() as usize;
    store
        .append_messages(
            &session_id,
            &session.messages()[stored..],
            Some(stored as u64),
        )
        .unwrap();
    assert_eq!(store.message_count(&session_id).unwrap(), 4);
}
