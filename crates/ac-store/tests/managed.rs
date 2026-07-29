use std::sync::{Arc, Barrier};

use ac_store::{
    ManagedClaim, ManagedEnqueue, ManagedPendingReorder, ManagedRecovery, ManagedRunAcquire,
    ManagedSteerCommit, ManagedSteerDelivery, ManagedSubmissionState, SqliteStore, StoreError,
};

fn store_with_session(id: &str) -> SqliteStore {
    let store = SqliteStore::open_in_memory().unwrap();
    assert!(store.create_session_with_id(id, None).unwrap());
    store
}

#[test]
fn durable_accept_is_idempotent_fifo_and_payload_safe() {
    let store = store_with_session("s");
    let first = store
        .enqueue_managed_submission("s", "a", r#"{"text":"one"}"#)
        .unwrap();
    let second = store
        .enqueue_managed_submission("s", "b", r#"{"text":"two"}"#)
        .unwrap();
    assert!(matches!(first, ManagedEnqueue::Inserted(_)));
    assert!(matches!(second, ManagedEnqueue::Inserted(_)));

    let retry = store
        .enqueue_managed_submission("s", "a", r#"{"text":"one"}"#)
        .unwrap();
    assert!(matches!(retry, ManagedEnqueue::Existing(_)));
    assert!(matches!(
        store.enqueue_managed_submission("s", "a", r#"{"text":"different"}"#),
        Err(StoreError::ManagedSubmissionConflict { .. })
    ));

    let pending = store.list_pending_managed_submissions("s").unwrap();
    assert_eq!(
        pending
            .iter()
            .map(|item| item.submission_id.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    assert_eq!(pending[0].sequence, 0);
    assert_eq!(pending[1].sequence, 1);
    assert_eq!(pending[0].queue_position, 0);
    assert_eq!(pending[1].queue_position, 1);
}

#[test]
fn pending_reorder_is_cas_idempotent_and_preserves_acceptance_sequence() {
    let store = store_with_session("s");
    for id in ["a", "b", "c"] {
        store.enqueue_managed_submission("s", id, id).unwrap();
    }
    let original = vec!["a".to_string(), "b".to_string(), "c".to_string()];
    let desired = vec!["c".to_string(), "a".to_string(), "b".to_string()];
    assert_eq!(
        store
            .reorder_pending_managed_submissions("s", &original, &desired)
            .unwrap(),
        ManagedPendingReorder::Reordered
    );
    let pending = store.list_pending_managed_submissions("s").unwrap();
    assert_eq!(
        pending
            .iter()
            .map(|record| record.submission_id.as_str())
            .collect::<Vec<_>>(),
        ["c", "a", "b"]
    );
    assert_eq!(
        pending
            .iter()
            .map(|record| record.sequence)
            .collect::<Vec<_>>(),
        [2, 0, 1]
    );
    assert_eq!(
        pending
            .iter()
            .map(|record| record.queue_position)
            .collect::<Vec<_>>(),
        [0, 1, 2]
    );
    assert_eq!(
        store
            .reorder_pending_managed_submissions("s", &original, &desired)
            .unwrap(),
        ManagedPendingReorder::Unchanged,
        "retrying the committed CAS must be idempotent"
    );
    let other = vec!["b".to_string(), "c".to_string(), "a".to_string()];
    assert_eq!(
        store
            .reorder_pending_managed_submissions("s", &original, &other)
            .unwrap(),
        ManagedPendingReorder::Conflict {
            current_order: desired.clone()
        }
    );
    assert!(matches!(
        store.reorder_pending_managed_submissions(
            "s",
            &original,
            &["a".to_string(), "a".to_string(), "c".to_string()]
        ),
        Err(StoreError::InvalidManagedPendingOrder(_))
    ));

    let ManagedClaim::Claimed { submission, .. } =
        store.claim_next_managed_submission("s", "r").unwrap()
    else {
        panic!("reordered head was not claimed");
    };
    assert_eq!(submission.submission_id, "c");
    assert_eq!(submission.sequence, 2);
}

fn running_head_with_steer(
    store: &SqliteStore,
    terminal: ManagedSubmissionState,
    commit_steer: bool,
) {
    store
        .enqueue_managed_submission("s", "head", "head")
        .unwrap();
    store
        .enqueue_managed_submission("s", "steer", "steer")
        .unwrap();
    assert!(matches!(
        store.claim_next_managed_submission("s", "run").unwrap(),
        ManagedClaim::Claimed { .. }
    ));
    assert!(store.mark_managed_input_committed("s", "run").unwrap());
    assert!(matches!(
        store
            .begin_pending_managed_steer("s", "steer", "run")
            .unwrap(),
        ManagedSteerCommit::Begun(_)
    ));
    assert!(matches!(
        store
            .begin_pending_managed_steer("s", "steer", "run")
            .unwrap(),
        ManagedSteerCommit::AlreadySteering(_)
    ));
    assert!(matches!(
        store
            .mark_pending_managed_steer_delivered("s", "steer", "run")
            .unwrap(),
        ManagedSteerDelivery::Delivered(_)
    ));
    assert!(matches!(
        store
            .mark_pending_managed_steer_delivered("s", "steer", "run")
            .unwrap(),
        ManagedSteerDelivery::AlreadyDelivered(_)
    ));
    let committed = commit_steer
        .then(|| "steer".to_string())
        .into_iter()
        .collect::<Vec<_>>();
    assert!(
        store
            .finish_managed_run("s", "run", terminal, None, &committed)
            .unwrap()
            .settled
    );
}

#[test]
fn settlement_commits_only_acknowledged_deliveries_independent_of_outcome() {
    for terminal in [
        ManagedSubmissionState::Succeeded,
        ManagedSubmissionState::Failed,
        ManagedSubmissionState::Cancelled,
    ] {
        let committed = store_with_session("s");
        running_head_with_steer(&committed, terminal, true);
        assert_eq!(
            committed
                .get_managed_submission("s", "steer")
                .unwrap()
                .unwrap()
                .state,
            ManagedSubmissionState::Steered
        );

        let store = store_with_session("s");
        running_head_with_steer(&store, terminal, false);
        let restored = store.get_managed_submission("s", "steer").unwrap().unwrap();
        assert_eq!(restored.state, ManagedSubmissionState::Pending);
        assert!(restored.run_id.is_none());
        assert_eq!(
            store
                .list_pending_managed_submissions("s")
                .unwrap()
                .iter()
                .map(|record| record.submission_id.as_str())
                .collect::<Vec<_>>(),
            ["steer"]
        );
    }
}

#[test]
fn direct_settlement_and_restart_resolve_steering_without_loss() {
    let completed = store_with_session("s");
    completed
        .enqueue_managed_submission("s", "steer", "steer")
        .unwrap();
    assert!(matches!(
        completed.try_acquire_managed_run("s", "direct").unwrap(),
        ManagedRunAcquire::Acquired(_)
    ));
    assert!(matches!(
        completed
            .begin_pending_managed_steer("s", "steer", "direct")
            .unwrap(),
        ManagedSteerCommit::Begun(_)
    ));
    assert!(matches!(
        completed
            .mark_pending_managed_steer_delivered("s", "steer", "direct")
            .unwrap(),
        ManagedSteerDelivery::Delivered(_)
    ));
    let settlement = completed
        .release_managed_run("s", "direct", &["steer".to_string()])
        .unwrap();
    assert!(settlement.settled);
    assert_eq!(
        settlement
            .steered
            .iter()
            .map(|record| record.submission_id.as_str())
            .collect::<Vec<_>>(),
        ["steer"]
    );
    assert_eq!(
        completed
            .get_managed_submission("s", "steer")
            .unwrap()
            .unwrap()
            .state,
        ManagedSubmissionState::Steered
    );

    for resolution in ["failed-direct", "restart"] {
        let store = store_with_session("s");
        store
            .enqueue_managed_submission("s", "steer", "steer")
            .unwrap();
        assert!(matches!(
            store.try_acquire_managed_run("s", resolution).unwrap(),
            ManagedRunAcquire::Acquired(_)
        ));
        assert!(matches!(
            store
                .begin_pending_managed_steer("s", "steer", resolution)
                .unwrap(),
            ManagedSteerCommit::Begun(_)
        ));
        assert!(matches!(
            store
                .mark_pending_managed_steer_delivered("s", "steer", resolution)
                .unwrap(),
            ManagedSteerDelivery::Delivered(_)
        ));
        if resolution == "restart" {
            let report = store.reconcile_managed_runs().unwrap();
            assert!(matches!(
                report.as_slice(),
                [ManagedRecovery::Released { .. }]
            ));
        } else {
            assert!(
                store
                    .release_managed_run("s", resolution, &[])
                    .unwrap()
                    .settled
            );
        }
        assert_eq!(
            store
                .get_managed_submission("s", "steer")
                .unwrap()
                .unwrap()
                .state,
            ManagedSubmissionState::Pending
        );
    }
}

#[test]
fn settlement_ack_is_unique_delivered_bound_and_failure_atomic() {
    let store = store_with_session("s");
    for id in ["head", "a", "b"] {
        store.enqueue_managed_submission("s", id, id).unwrap();
    }
    assert!(matches!(
        store.claim_next_managed_submission("s", "run").unwrap(),
        ManagedClaim::Claimed { .. }
    ));
    assert!(store.mark_managed_input_committed("s", "run").unwrap());
    for id in ["a", "b"] {
        assert!(matches!(
            store.begin_pending_managed_steer("s", id, "run").unwrap(),
            ManagedSteerCommit::Begun(_)
        ));
    }
    assert!(matches!(
        store
            .mark_pending_managed_steer_delivered("s", "a", "run")
            .unwrap(),
        ManagedSteerDelivery::Delivered(_)
    ));
    assert!(matches!(
        store
            .begin_pending_managed_steer("s", "a", "run")
            .unwrap(),
        ManagedSteerCommit::AlreadySteering(record)
            if record.state == ManagedSubmissionState::Delivered
    ));

    for invalid in [
        vec!["a".to_string(), "a".to_string()],
        vec!["b".to_string()],
        vec!["missing".to_string()],
    ] {
        assert!(matches!(
            store.finish_managed_run("s", "run", ManagedSubmissionState::Failed, None, &invalid,),
            Err(StoreError::InvalidManagedSteerSettlement(_))
        ));
        assert_eq!(
            store
                .get_managed_submission("s", "head")
                .unwrap()
                .unwrap()
                .state,
            ManagedSubmissionState::Running
        );
        assert_eq!(
            store.active_managed_run("s").unwrap().unwrap().run_id,
            "run"
        );
    }

    let settled = store
        .finish_managed_run(
            "s",
            "run",
            ManagedSubmissionState::Failed,
            Some("primary run failed after persisting a"),
            &["a".to_string()],
        )
        .unwrap();
    assert_eq!(
        settled
            .steered
            .iter()
            .map(|record| record.submission_id.as_str())
            .collect::<Vec<_>>(),
        ["a"]
    );
    assert_eq!(
        store
            .get_managed_submission("s", "a")
            .unwrap()
            .unwrap()
            .state,
        ManagedSubmissionState::Steered
    );
    assert_eq!(
        store
            .get_managed_submission("s", "b")
            .unwrap()
            .unwrap()
            .state,
        ManagedSubmissionState::Pending
    );
}

#[test]
fn settlement_returns_steers_in_ack_order_not_queue_order() {
    let store = store_with_session("s");
    for id in ["head", "a", "b"] {
        store.enqueue_managed_submission("s", id, id).unwrap();
    }
    assert!(matches!(
        store.claim_next_managed_submission("s", "run").unwrap(),
        ManagedClaim::Claimed { .. }
    ));
    assert!(store.mark_managed_input_committed("s", "run").unwrap());
    for id in ["a", "b"] {
        assert!(matches!(
            store.begin_pending_managed_steer("s", id, "run").unwrap(),
            ManagedSteerCommit::Begun(_)
        ));
        assert!(matches!(
            store
                .mark_pending_managed_steer_delivered("s", id, "run")
                .unwrap(),
            ManagedSteerDelivery::Delivered(_)
        ));
    }
    let settlement = store
        .finish_managed_run(
            "s",
            "run",
            ManagedSubmissionState::Succeeded,
            None,
            &["b".to_string(), "a".to_string()],
        )
        .unwrap();
    assert_eq!(
        settlement
            .steered
            .iter()
            .map(|record| record.submission_id.as_str())
            .collect::<Vec<_>>(),
        ["b", "a"]
    );
}

#[test]
fn reorder_preserves_hidden_steer_slot_when_uncommitted_delivery_returns() {
    let store = store_with_session("s");
    for id in ["a", "b", "c"] {
        store.enqueue_managed_submission("s", id, id).unwrap();
    }
    assert!(matches!(
        store.try_acquire_managed_run("s", "direct").unwrap(),
        ManagedRunAcquire::Acquired(_)
    ));
    assert!(matches!(
        store
            .begin_pending_managed_steer("s", "a", "direct")
            .unwrap(),
        ManagedSteerCommit::Begun(_)
    ));
    assert!(matches!(
        store
            .mark_pending_managed_steer_delivered("s", "a", "direct")
            .unwrap(),
        ManagedSteerDelivery::Delivered(_)
    ));
    assert_eq!(
        store
            .reorder_pending_managed_submissions(
                "s",
                &["b".to_string(), "c".to_string()],
                &["c".to_string(), "b".to_string()],
            )
            .unwrap(),
        ManagedPendingReorder::Reordered
    );
    assert!(
        store
            .release_managed_run("s", "direct", &[])
            .unwrap()
            .settled
    );
    assert_eq!(
        store
            .list_pending_managed_submissions("s")
            .unwrap()
            .iter()
            .map(|record| record.submission_id.as_str())
            .collect::<Vec<_>>(),
        ["a", "c", "b"]
    );
}

#[test]
fn panicking_mutation_listener_cannot_revoke_accept_claim_or_direct_lease() {
    let store = store_with_session("s");
    store.set_mutation_listener(Some(Arc::new(|_| {
        panic!("listener failure after commit");
    })));

    let accepted = store
        .enqueue_managed_submission("s", "a", "durable")
        .unwrap();
    assert!(matches!(accepted, ManagedEnqueue::Inserted(_)));
    assert_eq!(
        store
            .get_managed_submission("s", "a")
            .unwrap()
            .unwrap()
            .state,
        ManagedSubmissionState::Pending
    );

    let claimed = store.claim_next_managed_submission("s", "r").unwrap();
    assert!(matches!(claimed, ManagedClaim::Claimed { .. }));
    assert_eq!(store.active_managed_run("s").unwrap().unwrap().run_id, "r");

    assert!(store.create_session_with_id("direct", None).unwrap());
    assert!(matches!(
        store
            .try_acquire_managed_run("direct", "direct-run")
            .unwrap(),
        ManagedRunAcquire::Acquired(_)
    ));
    assert_eq!(
        store.active_managed_run("direct").unwrap().unwrap().run_id,
        "direct-run"
    );
}

#[test]
fn pending_cancel_never_reaches_a_claimed_or_terminal_item() {
    let store = store_with_session("s");
    store.enqueue_managed_submission("s", "a", "one").unwrap();
    store.enqueue_managed_submission("s", "b", "two").unwrap();
    assert!(store.cancel_pending_managed_submission("s", "b").unwrap());
    assert!(!store.cancel_pending_managed_submission("s", "b").unwrap());

    let ManagedClaim::Claimed { submission, .. } =
        store.claim_next_managed_submission("s", "r1").unwrap()
    else {
        panic!("oldest pending item was not claimed");
    };
    assert_eq!(submission.submission_id, "a");
    assert!(!store.cancel_pending_managed_submission("s", "a").unwrap());
    assert!(
        store
            .list_pending_managed_submissions("s")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn checked_claim_validation_failure_leaves_no_lease_or_state_change() {
    let store = store_with_session("s");
    store
        .enqueue_managed_submission("s", "a", "incompatible")
        .unwrap();

    let result = store.claim_next_managed_submission_checked("s", "r", |_| {
        Err(StoreError::InvalidManagedState(
            "host payload rejected".to_string(),
        ))
    });
    assert!(matches!(result, Err(StoreError::InvalidManagedState(_))));
    assert!(store.active_managed_run("s").unwrap().is_none());
    assert_eq!(
        store
            .get_managed_submission("s", "a")
            .unwrap()
            .unwrap()
            .state,
        ManagedSubmissionState::Pending
    );
}

#[test]
fn concurrent_claim_has_one_winner_and_stale_finish_cannot_release_it() {
    let store = Arc::new(store_with_session("s"));
    store.enqueue_managed_submission("s", "a", "one").unwrap();
    let barrier = Arc::new(Barrier::new(3));
    let mut workers = Vec::new();
    for run_id in ["r1", "r2"] {
        let store = store.clone();
        let barrier = barrier.clone();
        workers.push(std::thread::spawn(move || {
            barrier.wait();
            store.claim_next_managed_submission("s", run_id).unwrap()
        }));
    }
    barrier.wait();
    let outcomes: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, ManagedClaim::Claimed { .. }))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, ManagedClaim::Held(_)))
            .count(),
        1
    );

    let active = store.active_managed_run("s").unwrap().unwrap();
    let stale = if active.run_id == "r1" { "r2" } else { "r1" };
    assert!(!store.mark_managed_input_committed("s", stale).unwrap());
    assert!(
        !store
            .finish_managed_run("s", stale, ManagedSubmissionState::Succeeded, None, &[],)
            .unwrap()
            .settled
    );
    assert_eq!(
        store.active_managed_run("s").unwrap().unwrap().run_id,
        active.run_id
    );

    assert!(
        store
            .mark_managed_input_committed("s", &active.run_id)
            .unwrap()
    );
    assert!(
        store
            .finish_managed_run(
                "s",
                &active.run_id,
                ManagedSubmissionState::Succeeded,
                None,
                &[],
            )
            .unwrap()
            .settled
    );
    assert!(store.active_managed_run("s").unwrap().is_none());
    let record = store.get_managed_submission("s", "a").unwrap().unwrap();
    assert_eq!(record.state, ManagedSubmissionState::Succeeded);
}

#[test]
fn direct_runs_share_single_flight_with_submission_claims() {
    let store = store_with_session("s");
    store.enqueue_managed_submission("s", "a", "one").unwrap();
    assert!(matches!(
        store.try_acquire_managed_run("s", "maintenance").unwrap(),
        ManagedRunAcquire::Acquired(_)
    ));
    assert!(matches!(
        store.claim_next_managed_submission("s", "queued").unwrap(),
        ManagedClaim::Held(_)
    ));
    assert!(
        !store
            .release_managed_run("s", "stale", &[])
            .unwrap()
            .settled
    );
    assert!(
        store
            .release_managed_run("s", "maintenance", &[])
            .unwrap()
            .settled
    );
    assert!(matches!(
        store.claim_next_managed_submission("s", "queued").unwrap(),
        ManagedClaim::Claimed { .. }
    ));
}

#[test]
fn recovery_requeues_precommit_and_interrupts_postcommit_exactly_once() {
    let store = store_with_session("s");
    store.enqueue_managed_submission("s", "pre", "one").unwrap();
    assert!(matches!(
        store.claim_next_managed_submission("s", "r-pre").unwrap(),
        ManagedClaim::Claimed { .. }
    ));
    let first = store.reconcile_managed_runs().unwrap();
    assert!(matches!(
        first.as_slice(),
        [ManagedRecovery::Requeued { submission, .. }]
            if submission.submission_id == "pre"
    ));
    assert_eq!(
        store
            .get_managed_submission("s", "pre")
            .unwrap()
            .unwrap()
            .state,
        ManagedSubmissionState::Pending
    );

    assert!(matches!(
        store
            .claim_next_managed_submission("s", "r-running")
            .unwrap(),
        ManagedClaim::Claimed { .. }
    ));
    assert!(
        store
            .mark_managed_input_committed("s", "r-running")
            .unwrap()
    );
    let second = store.reconcile_managed_runs().unwrap();
    assert!(matches!(
        second.as_slice(),
        [ManagedRecovery::Interrupted { submission, .. }]
            if submission.submission_id == "pre"
    ));
    let record = store.get_managed_submission("s", "pre").unwrap().unwrap();
    assert_eq!(record.state, ManagedSubmissionState::Interrupted);
    assert!(record.error.as_deref().unwrap().contains("process restart"));
    assert!(store.reconcile_managed_runs().unwrap().is_empty());
}

#[test]
fn accepted_pending_survives_reopen_and_session_delete_cascades() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("managed.db");
    {
        let store = SqliteStore::open(&path).unwrap();
        assert!(store.create_session_with_id("s", None).unwrap());
        store
            .enqueue_managed_submission("s", "a", "durable")
            .unwrap();
    }
    {
        let store = SqliteStore::open(&path).unwrap();
        let pending = store.list_pending_managed_submissions("s").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].payload, "durable");
        assert!(store.delete_session("s").unwrap());
    }
    let raw = rusqlite::Connection::open(&path).unwrap();
    let submissions: i64 = raw
        .query_row("SELECT COUNT(*) FROM managed_submissions", [], |row| {
            row.get(0)
        })
        .unwrap();
    let runs: i64 = raw
        .query_row("SELECT COUNT(*) FROM managed_runs", [], |row| row.get(0))
        .unwrap();
    assert_eq!((submissions, runs), (0, 0));
}

#[test]
fn v2_store_migrates_to_current_without_losing_session_or_messages() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v2.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
               id TEXT PRIMARY KEY, title TEXT, meta TEXT,
               created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
             );
             CREATE TABLE messages (
               session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
               seq INTEGER NOT NULL, role TEXT NOT NULL, content TEXT NOT NULL,
               cache INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL,
               meta TEXT, PRIMARY KEY (session_id, seq)
             );
             INSERT INTO sessions VALUES ('s', 'kept', NULL, 1, 1);
             INSERT INTO messages VALUES (
               's', 0, 'user', '[{\"type\":\"text\",\"text\":\"kept\"}]', 0, 1, NULL
             );
             PRAGMA user_version = 2;",
        )
        .unwrap();
    }

    let store = SqliteStore::open(&path).unwrap();
    assert_eq!(store.load_messages("s").unwrap().len(), 1);
    store.enqueue_managed_submission("s", "a", "new").unwrap();
    drop(store);
    let raw = rusqlite::Connection::open(&path).unwrap();
    let version: u32 = raw
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 5);
}

#[test]
fn v3_shaped_store_with_an_old_stamp_migrates_by_shape_only_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v3.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
               id TEXT PRIMARY KEY, title TEXT, meta TEXT,
               created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
             );
             CREATE TABLE messages (
               session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
               seq INTEGER NOT NULL, role TEXT NOT NULL, content TEXT NOT NULL,
               cache INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL,
               meta TEXT, PRIMARY KEY (session_id, seq)
             );
             CREATE TABLE managed_submissions (
               session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
               sequence INTEGER NOT NULL,
               submission_id TEXT NOT NULL,
               payload TEXT NOT NULL,
               state TEXT NOT NULL CHECK (
                 state IN (
                   'pending', 'claimed', 'running', 'succeeded',
                   'failed', 'cancelled', 'interrupted'
                 )
               ),
               run_id TEXT,
               accepted_at INTEGER NOT NULL,
               started_at INTEGER,
               finished_at INTEGER,
               error TEXT,
               PRIMARY KEY (session_id, submission_id),
               UNIQUE (session_id, sequence)
             );
             CREATE TABLE managed_runs (
               session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
               run_id TEXT NOT NULL UNIQUE,
               submission_id TEXT,
               started_at INTEGER NOT NULL,
               FOREIGN KEY (session_id, submission_id)
                 REFERENCES managed_submissions(session_id, submission_id) ON DELETE CASCADE
             );
             INSERT INTO sessions VALUES ('s', NULL, NULL, 1, 1);
             INSERT INTO managed_submissions VALUES (
               's', 7, 'kept', 'opaque', 'pending', NULL, 2, NULL, NULL, NULL
             );
             PRAGMA user_version = 2;",
        )
        .unwrap();
    }

    let store = SqliteStore::open(&path).unwrap();
    let record = store.get_managed_submission("s", "kept").unwrap().unwrap();
    assert_eq!(record.sequence, 7);
    assert_eq!(record.queue_position, 7);
    drop(store);

    // Simulate a crash after the current table rebuild committed but before the
    // schema stamp advanced. Reopen must detect the column and preserve a
    // post-migration queue edit instead of rebuilding from sequence again.
    {
        let raw = rusqlite::Connection::open(&path).unwrap();
        raw.execute(
            "UPDATE managed_submissions SET queue_position = 1
             WHERE session_id = 's' AND submission_id = 'kept'",
            [],
        )
        .unwrap();
        raw.pragma_update(None, "user_version", 3).unwrap();
    }
    let reopened = SqliteStore::open(&path).unwrap();
    assert_eq!(
        reopened
            .get_managed_submission("s", "kept")
            .unwrap()
            .unwrap()
            .queue_position,
        1
    );
    drop(reopened);

    let raw = rusqlite::Connection::open(&path).unwrap();
    let version: u32 = raw
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 5);
    let foreign_key_errors: i64 = raw
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(foreign_key_errors, 0);
}

#[test]
fn v4_store_migration_preserves_reordered_positions_and_recovers_steering() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v4.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
               id TEXT PRIMARY KEY, title TEXT, meta TEXT,
               created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
             );
             CREATE TABLE messages (
               session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
               seq INTEGER NOT NULL, role TEXT NOT NULL, content TEXT NOT NULL,
               cache INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL,
               meta TEXT, PRIMARY KEY (session_id, seq)
             );
             CREATE TABLE managed_submissions (
               session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
               sequence INTEGER NOT NULL,
               queue_position INTEGER NOT NULL,
               submission_id TEXT NOT NULL,
               payload TEXT NOT NULL,
               state TEXT NOT NULL CHECK (
                 state IN (
                   'pending', 'claimed', 'running', 'steering', 'steered',
                   'succeeded', 'failed', 'cancelled', 'interrupted'
                 )
               ),
               run_id TEXT,
               accepted_at INTEGER NOT NULL,
               started_at INTEGER,
               finished_at INTEGER,
               error TEXT,
               PRIMARY KEY (session_id, submission_id),
               UNIQUE (session_id, sequence)
             );
             CREATE TABLE managed_runs (
               session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
               run_id TEXT NOT NULL UNIQUE,
               submission_id TEXT,
               started_at INTEGER NOT NULL,
               FOREIGN KEY (session_id, submission_id)
                 REFERENCES managed_submissions(session_id, submission_id) ON DELETE CASCADE
             );
             INSERT INTO sessions VALUES ('s', NULL, NULL, 1, 1);
             INSERT INTO managed_submissions VALUES
               ('s', 0, 0, 'head', 'head', 'running', 'run', 1, 2, NULL, NULL),
               ('s', 1, 7, 'steer', 'steer', 'steering', 'run', 1, 2, NULL, NULL),
               ('s', 2, 3, 'later', 'later', 'pending', NULL, 1, NULL, NULL, NULL);
             INSERT INTO managed_runs VALUES ('s', 'run', 'head', 2);
             PRAGMA user_version = 4;",
        )
        .unwrap();
    }

    let store = SqliteStore::open(&path).unwrap();
    assert_eq!(
        store
            .get_managed_submission("s", "steer")
            .unwrap()
            .unwrap()
            .queue_position,
        7
    );
    assert!(matches!(
        store.reconcile_managed_runs().unwrap().as_slice(),
        [ManagedRecovery::Interrupted { submission, .. }]
            if submission.submission_id == "head"
    ));
    let pending = store.list_pending_managed_submissions("s").unwrap();
    assert_eq!(
        pending
            .iter()
            .map(|record| (record.submission_id.as_str(), record.queue_position))
            .collect::<Vec<_>>(),
        [("later", 3), ("steer", 7)]
    );
    drop(store);

    let raw = rusqlite::Connection::open(&path).unwrap();
    let version: u32 = raw
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 5);
    let table_sql: String = raw
        .query_row(
            "SELECT sql FROM sqlite_master
             WHERE type = 'table' AND name = 'managed_submissions'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(table_sql.contains("'delivered'"));
    let foreign_key_errors: i64 = raw
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(foreign_key_errors, 0);
}
