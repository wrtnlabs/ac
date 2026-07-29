use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ac_managed::{
    ActiveRunRef, BoxError, DirectRunAcquire, DirectRunLease, ManagedClaim, ManagedEvent,
    ManagedObserver, ManagedPhase, ManagedRecovery, ManagedRunner, ManagedRuns, ManagedStore,
    PendingReorder, RunContext, RunOutcome, RunSettlement, SqliteManagedStore, SteerDelivery,
    SteerPending, StoreAccept, StoreDirectRunAcquire, StorePendingReorder, StoreSettlement,
    StoreSteerDelivery, StoreSteerReservation, Submission, SubmissionDisposition, SubmissionRecord,
    SubmissionState,
};
use ac_store::SqliteStore;
use futures::future::BoxFuture;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct TestRunner {
    commits: Mutex<Vec<String>>,
    starts: Mutex<Vec<String>>,
    outcomes: Mutex<HashMap<String, RunOutcome>>,
    settlements: Mutex<HashMap<String, RunSettlement>>,
    commit_blockers: Mutex<HashMap<String, oneshot::Receiver<()>>>,
    blockers: Mutex<HashMap<String, oneshot::Receiver<()>>>,
    steer_blockers: Mutex<HashMap<String, oneshot::Receiver<()>>>,
    steers: Mutex<Vec<(String, String)>>,
    unavailable_steers: Mutex<HashSet<String>>,
    failed_steers: Mutex<HashSet<String>>,
    fail_commit_once: Mutex<HashSet<String>>,
    active: AtomicUsize,
    max_active: AtomicUsize,
}

impl TestRunner {
    fn block_commit(&self, id: &str) -> oneshot::Sender<()> {
        let (tx, rx) = oneshot::channel();
        self.commit_blockers
            .lock()
            .unwrap()
            .insert(id.to_string(), rx);
        tx
    }

    fn block(&self, id: &str) -> oneshot::Sender<()> {
        let (tx, rx) = oneshot::channel();
        self.blockers.lock().unwrap().insert(id.to_string(), rx);
        tx
    }

    fn outcome(&self, id: &str, outcome: RunOutcome) {
        self.outcomes
            .lock()
            .unwrap()
            .insert(id.to_string(), outcome);
    }

    fn settlement(&self, id: &str, outcome: RunOutcome, committed_steer_ids: &[&str]) {
        self.settlements.lock().unwrap().insert(
            id.to_string(),
            RunSettlement::new(
                outcome,
                committed_steer_ids
                    .iter()
                    .map(|id| (*id).to_string())
                    .collect(),
            ),
        );
    }

    fn block_steer(&self, id: &str) -> oneshot::Sender<()> {
        let (tx, rx) = oneshot::channel();
        self.steer_blockers
            .lock()
            .unwrap()
            .insert(id.to_string(), rx);
        tx
    }

    fn steer_unavailable(&self, id: &str) {
        self.unavailable_steers
            .lock()
            .unwrap()
            .insert(id.to_string());
    }

    fn fail_steer(&self, id: &str) {
        self.failed_steers.lock().unwrap().insert(id.to_string());
    }

    fn fail_commit_once(&self, id: &str) {
        self.fail_commit_once.lock().unwrap().insert(id.to_string());
    }

    fn started(&self) -> Vec<String> {
        self.starts.lock().unwrap().clone()
    }
}

impl ManagedRunner<String> for TestRunner {
    fn commit_input(
        self: Arc<Self>,
        _run: RunContext,
        submission: SubmissionRecord<String>,
    ) -> BoxFuture<'static, Result<(), BoxError>> {
        Box::pin(async move {
            let id = submission.submission.id;
            self.commits.lock().unwrap().push(id.clone());
            let blocker = self.commit_blockers.lock().unwrap().remove(&id);
            if let Some(blocker) = blocker {
                let _ = blocker.await;
            }
            if self.fail_commit_once.lock().unwrap().remove(&id) {
                return Err(Box::new(TestError(format!("commit failed for {id}"))) as BoxError);
            }
            Ok(())
        })
    }

    fn run(
        self: Arc<Self>,
        run: RunContext,
        submission: SubmissionRecord<String>,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, RunSettlement> {
        Box::pin(async move {
            let id = submission.submission.id;
            if cancel.is_cancelled() {
                return RunOutcome::Cancelled.into();
            }
            self.starts.lock().unwrap().push(id.clone());
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);
            let blocker = self.blockers.lock().unwrap().remove(&id);
            if let Some(blocker) = blocker {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        self.active.fetch_sub(1, Ordering::SeqCst);
                        return RunOutcome::Cancelled.into();
                    }
                    _ = blocker => {}
                }
            }
            self.active.fetch_sub(1, Ordering::SeqCst);
            if let Some(settlement) = self.settlements.lock().unwrap().remove(&id) {
                return settlement;
            }
            let outcome = self
                .outcomes
                .lock()
                .unwrap()
                .remove(&id)
                .unwrap_or(RunOutcome::Completed);
            let committed_steer_ids = if outcome == RunOutcome::Completed {
                self.steers
                    .lock()
                    .unwrap()
                    .iter()
                    .filter_map(|(run_id, id)| (run_id == &run.run_id).then_some(id.clone()))
                    .collect()
            } else {
                Vec::new()
            };
            RunSettlement::new(outcome, committed_steer_ids)
        })
    }

    fn steer(
        self: Arc<Self>,
        run: ActiveRunRef,
        submission: SubmissionRecord<String>,
    ) -> BoxFuture<'static, Result<SteerDelivery, BoxError>> {
        Box::pin(async move {
            let id = submission.submission.id;
            let blocker = self.steer_blockers.lock().unwrap().remove(&id);
            if let Some(blocker) = blocker {
                let _ = blocker.await;
            }
            if self.unavailable_steers.lock().unwrap().remove(&id) {
                return Ok(SteerDelivery::Unavailable);
            }
            if self.failed_steers.lock().unwrap().remove(&id) {
                return Err(Box::new(TestError(format!("steer failed for {id}"))) as BoxError);
            }
            self.steers.lock().unwrap().push((run.run_id, id));
            Ok(SteerDelivery::Accepted)
        })
    }
}

#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<ManagedEvent<String>>>,
}

impl ManagedObserver<String> for Recorder {
    fn observe(&self, event: &ManagedEvent<String>) {
        self.events.lock().unwrap().push(event.clone());
    }
}

struct PanickingObserver;

impl ManagedObserver<String> for PanickingObserver {
    fn observe(&self, _event: &ManagedEvent<String>) {
        panic!("observer failure");
    }
}

struct BlockingFirstQueueObserver {
    queue_calls: AtomicUsize,
    block_next: AtomicBool,
    first_entered: Mutex<Option<oneshot::Sender<()>>>,
    release_first: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    snapshots: Mutex<Vec<Vec<String>>>,
}

impl ManagedObserver<String> for BlockingFirstQueueObserver {
    fn observe(&self, event: &ManagedEvent<String>) {
        let ManagedEvent::QueueChanged { pending, .. } = event else {
            return;
        };
        let snapshot = pending
            .iter()
            .map(|record| record.submission.id.clone())
            .collect::<Vec<_>>();
        self.queue_calls.fetch_add(1, Ordering::SeqCst);
        if self.block_next.swap(false, Ordering::SeqCst) {
            if let Some(entered) = self.first_entered.lock().unwrap().take() {
                let _ = entered.send(());
            }
            if let Some(release) = self.release_first.lock().unwrap().take() {
                let _ = release.recv();
            }
        }
        // Record after the deliberate block. Without the production
        // publication gate, a newer concurrent snapshot can overtake this one
        // and make the observed queue regress.
        self.snapshots.lock().unwrap().push(snapshot);
    }
}

#[derive(Clone, Copy)]
enum BlockLifecycle {
    InputCommitFailed,
    SteerDelivered,
    RunSettled,
    DirectRunStarted,
    DirectRunSettled,
}

struct BlockingLifecycleObserver {
    target: BlockLifecycle,
    target_id: String,
    entered: Mutex<Option<oneshot::Sender<()>>>,
    release: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    events: Mutex<Vec<ManagedEvent<String>>>,
}

impl BlockingLifecycleObserver {
    fn blocks(&self, event: &ManagedEvent<String>) -> bool {
        match (self.target, event) {
            (BlockLifecycle::InputCommitFailed, ManagedEvent::InputCommitFailed { run, .. })
            | (BlockLifecycle::RunSettled, ManagedEvent::RunSettled { run, .. }) => {
                run.submission_id == self.target_id
            }
            (BlockLifecycle::SteerDelivered, ManagedEvent::SteerDelivered { submission, .. }) => {
                submission.submission.id == self.target_id
            }
            (BlockLifecycle::DirectRunStarted, ManagedEvent::DirectRunStarted { run }) => {
                run.run_id == self.target_id
            }
            (BlockLifecycle::DirectRunSettled, ManagedEvent::DirectRunSettled { run, .. }) => {
                run.run_id == self.target_id
            }
            _ => false,
        }
    }
}

impl ManagedObserver<String> for BlockingLifecycleObserver {
    fn observe(&self, event: &ManagedEvent<String>) {
        if self.blocks(event)
            && let Some(entered) = self.entered.lock().unwrap().take()
        {
            let _ = entered.send(());
            if let Some(release) = self.release.lock().unwrap().take() {
                let _ = release.recv();
            }
        }
        self.events.lock().unwrap().push(event.clone());
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct TestError(String);

struct OneShotStoreBlock {
    entered: Mutex<Option<oneshot::Sender<()>>>,
    release: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}

impl OneShotStoreBlock {
    fn new(entered: oneshot::Sender<()>, release: std::sync::mpsc::Receiver<()>) -> Self {
        Self {
            entered: Mutex::new(Some(entered)),
            release: Mutex::new(Some(release)),
        }
    }

    fn wait(&self) {
        if let Some(entered) = self.entered.lock().unwrap().take() {
            let _ = entered.send(());
        }
        if let Some(release) = self.release.lock().unwrap().take() {
            let _ = release.recv();
        }
    }
}

struct TransitionFaultStore {
    inner: SqliteManagedStore<String>,
    fail_requeue_after_commit: AtomicBool,
    fail_finish_after_commit: AtomicBool,
    fail_mark_delivered_before_commit: AtomicBool,
    mark_delivered_attempts: AtomicUsize,
    mark_delivered_block: Option<OneShotStoreBlock>,
    accept_block: Option<(String, OneShotStoreBlock)>,
    claim_block: Option<OneShotStoreBlock>,
}

impl TransitionFaultStore {
    fn new(inner: Arc<SqliteStore>, fail_requeue: bool, fail_finish: bool) -> Self {
        Self {
            inner: SqliteManagedStore::new(inner),
            fail_requeue_after_commit: AtomicBool::new(fail_requeue),
            fail_finish_after_commit: AtomicBool::new(fail_finish),
            fail_mark_delivered_before_commit: AtomicBool::new(false),
            mark_delivered_attempts: AtomicUsize::new(0),
            mark_delivered_block: None,
            accept_block: None,
            claim_block: None,
        }
    }

    fn blocking_failed_mark_delivered_once(
        inner: Arc<SqliteStore>,
        entered: oneshot::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
    ) -> Self {
        let mut store = Self::new(inner, false, false);
        store
            .fail_mark_delivered_before_commit
            .store(true, Ordering::SeqCst);
        store.mark_delivered_block = Some(OneShotStoreBlock::new(entered, release));
        store
    }

    fn blocking_accept(
        inner: Arc<SqliteStore>,
        submission_id: &str,
        entered: oneshot::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
    ) -> Self {
        let mut store = Self::new(inner, false, false);
        store.accept_block = Some((
            submission_id.to_string(),
            OneShotStoreBlock::new(entered, release),
        ));
        store
    }

    fn blocking_claim(
        inner: Arc<SqliteStore>,
        entered: oneshot::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
    ) -> Self {
        let mut store = Self::new(inner, false, false);
        store.claim_block = Some(OneShotStoreBlock::new(entered, release));
        store
    }
}

impl ManagedStore<String> for TransitionFaultStore {
    fn accept(
        &self,
        session_id: &str,
        submission: &Submission<String>,
    ) -> Result<StoreAccept<String>, BoxError> {
        let accepted = self.inner.accept(session_id, submission)?;
        if let Some((blocked_id, block)) = &self.accept_block
            && blocked_id == &submission.id
        {
            block.wait();
        }
        Ok(accepted)
    }

    fn get(
        &self,
        session_id: &str,
        submission_id: &str,
    ) -> Result<Option<SubmissionRecord<String>>, BoxError> {
        self.inner.get(session_id, submission_id)
    }

    fn pending(&self, session_id: &str) -> Result<Vec<SubmissionRecord<String>>, BoxError> {
        self.inner.pending(session_id)
    }

    fn pending_sessions(&self) -> Result<Vec<String>, BoxError> {
        self.inner.pending_sessions()
    }

    fn cancel_pending(&self, session_id: &str, submission_id: &str) -> Result<bool, BoxError> {
        self.inner.cancel_pending(session_id, submission_id)
    }

    fn reorder_pending(
        &self,
        session_id: &str,
        expected_order: &[String],
        desired_order: &[String],
    ) -> Result<StorePendingReorder, BoxError> {
        self.inner
            .reorder_pending(session_id, expected_order, desired_order)
    }

    fn begin_steer(
        &self,
        session_id: &str,
        submission_id: &str,
        run_id: &str,
    ) -> Result<StoreSteerReservation<String>, BoxError> {
        self.inner.begin_steer(session_id, submission_id, run_id)
    }

    fn mark_steer_delivered(
        &self,
        session_id: &str,
        submission_id: &str,
        run_id: &str,
    ) -> Result<StoreSteerDelivery<String>, BoxError> {
        self.mark_delivered_attempts.fetch_add(1, Ordering::SeqCst);
        if self
            .fail_mark_delivered_before_commit
            .swap(false, Ordering::SeqCst)
        {
            if let Some(block) = &self.mark_delivered_block {
                block.wait();
            }
            return Err(Box::new(TestError(
                "delivery confirmation failed before commit".to_string(),
            )));
        }
        self.inner
            .mark_steer_delivered(session_id, submission_id, run_id)
    }

    fn rollback_steer(
        &self,
        session_id: &str,
        submission_id: &str,
        run_id: &str,
        error: &str,
    ) -> Result<bool, BoxError> {
        self.inner
            .rollback_steer(session_id, submission_id, run_id, error)
    }

    fn claim_next(&self, session_id: &str, run_id: &str) -> Result<ManagedClaim<String>, BoxError> {
        let claimed = self.inner.claim_next(session_id, run_id)?;
        if matches!(claimed, ManagedClaim::Claimed { .. })
            && let Some(block) = &self.claim_block
        {
            block.wait();
        }
        Ok(claimed)
    }

    fn mark_input_committed(&self, run: &RunContext) -> Result<bool, BoxError> {
        self.inner.mark_input_committed(run)
    }

    fn try_acquire_direct_run(
        &self,
        session_id: &str,
        run_id: &str,
    ) -> Result<StoreDirectRunAcquire, BoxError> {
        self.inner.try_acquire_direct_run(session_id, run_id)
    }

    fn release_direct_run(
        &self,
        run: &DirectRunLease,
        settlement: &RunSettlement,
    ) -> Result<StoreSettlement<String>, BoxError> {
        self.inner.release_direct_run(run, settlement)
    }

    fn requeue_claim(&self, run: &RunContext, error: &str) -> Result<bool, BoxError> {
        let changed = self.inner.requeue_claim(run, error)?;
        if changed && self.fail_requeue_after_commit.swap(false, Ordering::SeqCst) {
            return Err(Box::new(TestError(
                "requeue committed before injected fault".to_string(),
            )));
        }
        Ok(changed)
    }

    fn finish(
        &self,
        run: &RunContext,
        settlement: &RunSettlement,
    ) -> Result<StoreSettlement<String>, BoxError> {
        let stored = self.inner.finish(run, settlement)?;
        if stored.settled && self.fail_finish_after_commit.swap(false, Ordering::SeqCst) {
            return Err(Box::new(TestError(
                "settlement committed before injected fault".to_string(),
            )));
        }
        Ok(stored)
    }

    fn reconcile(&self) -> Result<Vec<ManagedRecovery<String>>, BoxError> {
        self.inner.reconcile()
    }
}

fn service(
    store: Arc<SqliteStore>,
    runner: Arc<TestRunner>,
    observer: Arc<Recorder>,
) -> Arc<ManagedRuns<String>> {
    ManagedRuns::new(Arc::new(SqliteManagedStore::new(store)), runner, observer)
}

fn submit(id: &str) -> Submission<String> {
    Submission {
        id: id.to_string(),
        payload: format!("payload:{id}"),
    }
}

async fn wait_for(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("condition timed out");
}

async fn acquire_direct_eventually(
    runs: &Arc<ManagedRuns<String>>,
    session_id: &str,
    run_id: &str,
) -> DirectRunLease {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match runs
                .try_acquire_direct_run(session_id, run_id)
                .await
                .unwrap()
            {
                DirectRunAcquire::Acquired(run) => return run,
                DirectRunAcquire::Held { .. } => {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                DirectRunAcquire::Quiescing => {
                    panic!("scheduler quiesced before direct acquisition")
                }
            }
        }
    })
    .await
    .expect("direct acquisition timed out")
}

#[tokio::test]
async fn idle_submit_starts_and_later_submissions_drain_fifo_one_at_a_time() {
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    store.create_session_with_id("s", None).unwrap();
    let runner = Arc::new(TestRunner::default());
    let release_first = runner.block("a");
    let observer = Arc::new(Recorder::default());
    let runs = service(store, runner.clone(), observer.clone());

    let first = runs.submit("s", submit("a")).await.unwrap();
    assert!(matches!(
        first.disposition,
        SubmissionDisposition::Started { .. }
    ));
    wait_for(|| runner.started() == ["a"]).await;
    assert!(matches!(
        runs.submit("s", submit("b")).await.unwrap().disposition,
        SubmissionDisposition::Queued { .. }
    ));
    assert!(matches!(
        runs.submit("s", submit("c")).await.unwrap().disposition,
        SubmissionDisposition::Queued { .. }
    ));
    assert_eq!(
        runs.pending("s")
            .await
            .unwrap()
            .iter()
            .map(|record| record.submission.id.as_str())
            .collect::<Vec<_>>(),
        ["b", "c"]
    );

    release_first.send(()).unwrap();
    wait_for(|| runner.started().len() == 3).await;
    wait_for(|| {
        observer
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| matches!(event, ManagedEvent::RunSettled { .. }))
            .count()
            == 3
    })
    .await;
    assert_eq!(runner.started(), ["a", "b", "c"]);
    assert_eq!(runner.max_active.load(Ordering::SeqCst), 1);
    assert!(runs.pending("s").await.unwrap().is_empty());
}

#[tokio::test]
async fn exact_submission_lookup_returns_the_durable_record() {
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    store.create_session_with_id("s", None).unwrap();
    let runs = service(
        store,
        Arc::new(TestRunner::default()),
        Arc::new(Recorder::default()),
    );
    runs.begin_quiesce().await;
    runs.submit("s", submit("pending")).await.unwrap();

    let record = runs
        .get("s", "pending")
        .await
        .unwrap()
        .expect("accepted record");
    assert_eq!(record.session_id, "s");
    assert_eq!(record.submission, submit("pending"));
    assert_eq!(record.state, SubmissionState::Pending);
    assert_eq!(runs.get("s", "missing").await.unwrap(), None);
}

#[tokio::test]
async fn durable_reorder_changes_claim_order_without_changing_acceptance_sequence() {
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    store.create_session_with_id("s", None).unwrap();
    let runner = Arc::new(TestRunner::default());
    let release_head = runner.block("head");
    let observer = Arc::new(Recorder::default());
    let runs = service(store, runner.clone(), observer.clone());

    runs.submit("s", submit("head")).await.unwrap();
    wait_for(|| runner.started() == ["head"]).await;
    for id in ["a", "b", "c"] {
        runs.submit("s", submit(id)).await.unwrap();
    }
    let expected = vec!["a".to_string(), "b".to_string(), "c".to_string()];
    let desired = vec!["c".to_string(), "a".to_string(), "b".to_string()];
    assert_eq!(
        runs.reorder_pending("s", expected.clone(), desired.clone())
            .await
            .unwrap(),
        PendingReorder::Reordered
    );
    let pending = runs.pending("s").await.unwrap();
    assert_eq!(
        pending
            .iter()
            .map(|record| record.submission.id.as_str())
            .collect::<Vec<_>>(),
        ["c", "a", "b"]
    );
    assert_eq!(
        pending
            .iter()
            .map(|record| record.sequence)
            .collect::<Vec<_>>(),
        [3, 1, 2],
        "acceptance order must remain immutable"
    );
    assert_eq!(
        runs.reorder_pending("s", expected.clone(), desired.clone())
            .await
            .unwrap(),
        PendingReorder::Unchanged
    );
    let other = vec!["b".to_string(), "c".to_string(), "a".to_string()];
    assert_eq!(
        runs.reorder_pending("s", expected, other).await.unwrap(),
        PendingReorder::Conflict {
            current_order: desired.clone()
        }
    );
    assert!(observer.events.lock().unwrap().iter().rev().any(|event| {
        matches!(
            event,
            ManagedEvent::QueueChanged { pending, .. }
                if pending
                    .iter()
                    .map(|record| record.submission.id.clone())
                    .collect::<Vec<_>>()
                    == desired
        )
    }));

    release_head.send(()).unwrap();
    wait_for(|| runner.started().len() == 4).await;
    assert_eq!(runner.started(), ["head", "c", "a", "b"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn steer_delivery_event_is_durable_synchronous_and_exactly_once() {
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    store.create_session_with_id("s", None).unwrap();
    let runner = Arc::new(TestRunner::default());
    let release_head = runner.block("head");
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let observer = Arc::new(BlockingLifecycleObserver {
        target: BlockLifecycle::SteerDelivered,
        target_id: "steer".to_string(),
        entered: Mutex::new(Some(entered_tx)),
        release: Mutex::new(Some(release_rx)),
        events: Mutex::new(Vec::new()),
    });
    let runs = ManagedRuns::new(
        Arc::new(SqliteManagedStore::new(store)),
        runner.clone(),
        observer.clone(),
    );
    runs.submit("s", submit("head")).await.unwrap();
    wait_for(|| runner.started() == ["head"]).await;
    runs.submit("s", submit("steer")).await.unwrap();

    let steer_runs = runs.clone();
    let steer_task = tokio::spawn(async move { steer_runs.steer_pending("s", "steer").await });
    entered_rx.await.unwrap();
    assert_eq!(
        runs.get("s", "steer")
            .await
            .unwrap()
            .expect("delivered record")
            .state,
        SubmissionState::Delivered,
        "the observer must run only after durable delivery confirmation"
    );
    assert!(
        !steer_task.is_finished(),
        "steer_pending returned before its synchronous delivery event completed"
    );

    release_tx.send(()).unwrap();
    let first = steer_task.await.unwrap().unwrap();
    let SteerPending::Steered { run_id } = first else {
        panic!("accepted steer did not report success");
    };
    assert_eq!(
        runs.steer_pending("s", "steer").await.unwrap(),
        SteerPending::AlreadySteered {
            run_id: run_id.clone()
        }
    );
    assert_eq!(
        observer
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    ManagedEvent::SteerDelivered { run, submission }
                        if run.run_id == run_id
                            && submission.submission.id == "steer"
                            && submission.state == SubmissionState::Delivered
                )
            })
            .count(),
        1,
        "a repeated AlreadySteered request must not republish delivery"
    );

    release_head.send(()).unwrap();
}

#[tokio::test]
async fn rejected_steers_do_not_publish_delivery_events() {
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    store.create_session_with_id("s", None).unwrap();
    let runner = Arc::new(TestRunner::default());
    let release_head = runner.block("head");
    runner.steer_unavailable("unavailable");
    runner.fail_steer("error");
    let observer = Arc::new(Recorder::default());
    let runs = service(store, runner.clone(), observer.clone());
    runs.submit("s", submit("head")).await.unwrap();
    wait_for(|| runner.started() == ["head"]).await;
    runs.submit("s", submit("unavailable")).await.unwrap();
    runs.submit("s", submit("error")).await.unwrap();

    assert!(matches!(
        runs.steer_pending("s", "unavailable").await.unwrap(),
        SteerPending::Unavailable { .. }
    ));
    let error = runs.steer_pending("s", "error").await.unwrap_err();
    assert_eq!(error.phase, ManagedPhase::SteerPending);
    assert_eq!(error.submission_id.as_deref(), Some("error"));
    assert!(
        observer
            .events
            .lock()
            .unwrap()
            .iter()
            .all(|event| !matches!(event, ManagedEvent::SteerDelivered { .. }))
    );

    assert!(runs.cancel_pending("s", "unavailable").await.unwrap());
    assert!(runs.cancel_pending("s", "error").await.unwrap());
    release_head.send(()).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropping_steer_caller_after_reservation_cannot_strand_or_lose_input() {
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    store.create_session_with_id("s", None).unwrap();
    let runner = Arc::new(TestRunner::default());
    let release_head = runner.block("head");
    let release_steer = runner.block_steer("steer");
    let observer = Arc::new(Recorder::default());
    let runs = service(store.clone(), runner.clone(), observer.clone());
    runs.submit("s", submit("head")).await.unwrap();
    wait_for(|| runner.started() == ["head"]).await;
    runs.submit("s", submit("steer")).await.unwrap();

    let steer_runs = runs.clone();
    let caller = tokio::spawn(async move { steer_runs.steer_pending("s", "steer").await });
    wait_for(|| {
        store
            .get_managed_submission("s", "steer")
            .unwrap()
            .is_some_and(|record| record.state == SubmissionState::Steering)
    })
    .await;
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    release_steer.send(()).unwrap();
    wait_for(|| runner.steers.lock().unwrap().len() == 1).await;
    let run_id = store.active_managed_run("s").unwrap().unwrap().run_id;
    assert_eq!(
        runs.steer_pending("s", "steer").await.unwrap(),
        SteerPending::AlreadySteered { run_id }
    );
    assert_eq!(
        store
            .get_managed_submission("s", "steer")
            .unwrap()
            .unwrap()
            .state,
        SubmissionState::Delivered
    );

    release_head.send(()).unwrap();
    wait_for(|| {
        store
            .get_managed_submission("s", "steer")
            .unwrap()
            .is_some_and(|record| record.state == SubmissionState::Steered)
    })
    .await;
    assert_eq!(runner.started(), ["head"]);
    assert!(observer.events.lock().unwrap().iter().any(|event| {
        matches!(
            event,
            ManagedEvent::RunSettled { steered, .. }
                if steered.iter().any(|record| record.submission.id == "steer")
        )
    }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn accepted_steer_retries_delivery_confirmation_before_releasing_settlement() {
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    store.create_session_with_id("s", None).unwrap();
    let runner = Arc::new(TestRunner::default());
    let release_head = runner.block("head");
    let observer = Arc::new(Recorder::default());
    let (mark_entered_tx, mark_entered_rx) = oneshot::channel();
    let (release_mark_tx, release_mark_rx) = std::sync::mpsc::channel();
    let fault_store = Arc::new(TransitionFaultStore::blocking_failed_mark_delivered_once(
        store.clone(),
        mark_entered_tx,
        release_mark_rx,
    ));
    let runs = ManagedRuns::new(fault_store.clone(), runner.clone(), observer.clone());
    runs.submit("s", submit("head")).await.unwrap();
    wait_for(|| runner.started() == ["head"]).await;
    runs.submit("s", submit("steer")).await.unwrap();

    let steer_runs = runs.clone();
    let steer_task = tokio::spawn(async move { steer_runs.steer_pending("s", "steer").await });
    mark_entered_rx.await.unwrap();
    assert_eq!(
        store
            .get_managed_submission("s", "steer")
            .unwrap()
            .unwrap()
            .state,
        SubmissionState::Steering
    );
    release_head.send(()).unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(
        store.active_managed_run("s").unwrap().is_some(),
        "settlement overtook delivery confirmation"
    );
    assert!(
        observer
            .events
            .lock()
            .unwrap()
            .iter()
            .all(|event| !matches!(event, ManagedEvent::RunSettled { .. })),
        "terminal publication overtook delivery confirmation"
    );

    release_mark_tx.send(()).unwrap();
    assert!(matches!(
        steer_task.await.unwrap().unwrap(),
        SteerPending::Steered { .. }
    ));
    assert!(fault_store.mark_delivered_attempts.load(Ordering::SeqCst) >= 2);
    assert!(
        matches!(
            store
                .get_managed_submission("s", "steer")
                .unwrap()
                .unwrap()
                .state,
            SubmissionState::Delivered | SubmissionState::Steered
        ),
        "delivery confirmation did not converge before settlement"
    );
    assert_eq!(
        observer
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    ManagedEvent::Fault(fault)
                        if fault.phase == ManagedPhase::SteerPending
                            && fault.submission_id.as_deref() == Some("steer")
                )
            })
            .count(),
        1,
        "transient confirmation retries should not spam observers"
    );

    wait_for(|| {
        store
            .get_managed_submission("s", "steer")
            .unwrap()
            .is_some_and(|record| record.state == SubmissionState::Steered)
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_run_can_commit_delivered_steers_in_proven_persistence_order() {
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    store.create_session_with_id("s", None).unwrap();
    let runner = Arc::new(TestRunner::default());
    let release_head = runner.block("head");
    runner.settlement(
        "head",
        RunOutcome::Failed {
            message: Some("failed after persisting steers".to_string()),
        },
        &["b", "a"],
    );
    let observer = Arc::new(Recorder::default());
    let runs = service(store.clone(), runner.clone(), observer.clone());
    runs.submit("s", submit("head")).await.unwrap();
    wait_for(|| runner.started() == ["head"]).await;
    runs.submit("s", submit("a")).await.unwrap();
    runs.submit("s", submit("b")).await.unwrap();
    assert!(matches!(
        runs.steer_pending("s", "a").await.unwrap(),
        SteerPending::Steered { .. }
    ));
    assert!(matches!(
        runs.steer_pending("s", "b").await.unwrap(),
        SteerPending::Steered { .. }
    ));
    release_head.send(()).unwrap();

    wait_for(|| {
        observer.events.lock().unwrap().iter().any(|event| {
            matches!(
                event,
                ManagedEvent::RunSettled {
                    outcome: RunOutcome::Failed { .. },
                    steered,
                    ..
                } if steered
                    .iter()
                    .map(|record| record.submission.id.as_str())
                    .collect::<Vec<_>>()
                    == ["b", "a"]
            )
        })
    })
    .await;
    for id in ["a", "b"] {
        assert_eq!(
            store
                .get_managed_submission("s", id)
                .unwrap()
                .unwrap()
                .state,
            SubmissionState::Steered
        );
    }
}

#[tokio::test]
async fn unavailable_steer_is_restored_to_the_pending_queue() {
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    store.create_session_with_id("s", None).unwrap();
    let runner = Arc::new(TestRunner::default());
    let release_head = runner.block("head");
    runner.steer_unavailable("steer");
    let runs = service(store.clone(), runner.clone(), Arc::new(Recorder::default()));
    runs.submit("s", submit("head")).await.unwrap();
    wait_for(|| runner.started() == ["head"]).await;
    runs.submit("s", submit("steer")).await.unwrap();
    let run_id = store.active_managed_run("s").unwrap().unwrap().run_id;
    assert_eq!(
        runs.steer_pending("s", "steer").await.unwrap(),
        SteerPending::Unavailable { run_id }
    );
    assert_eq!(
        store
            .get_managed_submission("s", "steer")
            .unwrap()
            .unwrap()
            .state,
        SubmissionState::Pending
    );
    release_head.send(()).unwrap();
}

#[tokio::test]
async fn steer_reports_no_active_and_terminal_submission_without_mutating_them() {
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    store.create_session_with_id("s", None).unwrap();
    let runs = service(
        store.clone(),
        Arc::new(TestRunner::default()),
        Arc::new(Recorder::default()),
    );
    runs.begin_quiesce().await;
    runs.submit("s", submit("pending")).await.unwrap();
    assert_eq!(
        runs.steer_pending("s", "pending").await.unwrap(),
        SteerPending::NoActiveRun
    );
    assert!(runs.cancel_pending("s", "pending").await.unwrap());
    assert_eq!(
        runs.steer_pending("s", "pending").await.unwrap(),
        SteerPending::NotPending {
            state: SubmissionState::Cancelled
        }
    );
    assert_eq!(
        runs.steer_pending("s", "missing").await.unwrap(),
        SteerPending::NotFound
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_and_cancelled_runs_restore_unacknowledged_steers_and_publish_the_queue() {
    for outcome in [
        RunOutcome::Failed {
            message: Some("failed".to_string()),
        },
        RunOutcome::Cancelled,
    ] {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        store.create_session_with_id("s", None).unwrap();
        let runner = Arc::new(TestRunner::default());
        let release_commit = runner.block_commit("head");
        runner.outcome("head", outcome);
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let observer = Arc::new(BlockingLifecycleObserver {
            target: BlockLifecycle::RunSettled,
            target_id: "head".to_string(),
            entered: Mutex::new(Some(entered_tx)),
            release: Mutex::new(Some(release_rx)),
            events: Mutex::new(Vec::new()),
        });
        let runs = ManagedRuns::new(
            Arc::new(SqliteManagedStore::new(store.clone())),
            runner,
            observer.clone(),
        );
        runs.submit("s", submit("head")).await.unwrap();
        runs.submit("s", submit("steer")).await.unwrap();
        assert!(matches!(
            runs.steer_pending("s", "steer").await.unwrap(),
            SteerPending::Steered { .. }
        ));
        release_commit.send(()).unwrap();
        entered_rx.await.unwrap();
        assert_eq!(
            store
                .get_managed_submission("s", "steer")
                .unwrap()
                .unwrap()
                .state,
            SubmissionState::Pending
        );
        runs.begin_quiesce().await;
        release_tx.send(()).unwrap();
        wait_for(|| {
            observer.events.lock().unwrap().iter().any(|event| {
                matches!(
                    event,
                    ManagedEvent::QueueChanged { pending, .. }
                        if pending.iter().any(|record| record.submission.id == "steer")
                )
            })
        })
        .await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn steer_during_terminal_publication_reports_no_active_run() {
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    store.create_session_with_id("s", None).unwrap();
    let runner = Arc::new(TestRunner::default());
    let release_head = runner.block("head");
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let observer = Arc::new(BlockingLifecycleObserver {
        target: BlockLifecycle::RunSettled,
        target_id: "head".to_string(),
        entered: Mutex::new(Some(entered_tx)),
        release: Mutex::new(Some(release_rx)),
        events: Mutex::new(Vec::new()),
    });
    let runs = ManagedRuns::new(
        Arc::new(SqliteManagedStore::new(store.clone())),
        runner.clone(),
        observer,
    );
    runs.submit("s", submit("head")).await.unwrap();
    wait_for(|| runner.started() == ["head"]).await;
    runs.submit("s", submit("pending")).await.unwrap();
    release_head.send(()).unwrap();
    entered_rx.await.unwrap();
    assert_eq!(
        runs.steer_pending("s", "pending").await.unwrap(),
        SteerPending::NoActiveRun
    );
    assert_eq!(
        store
            .get_managed_submission("s", "pending")
            .unwrap()
            .unwrap()
            .state,
        SubmissionState::Pending
    );
    runs.begin_quiesce().await;
    release_tx.send(()).unwrap();
}

#[tokio::test]
async fn failed_direct_run_restores_unacknowledged_steers_and_publishes_the_queue() {
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    store.create_session_with_id("s", None).unwrap();
    let runner = Arc::new(TestRunner::default());
    let observer = Arc::new(Recorder::default());
    let runs = service(store.clone(), runner, observer.clone());
    let direct = acquire_direct_eventually(&runs, "s", "direct").await;
    runs.submit("s", submit("steer")).await.unwrap();
    assert!(matches!(
        runs.steer_pending("s", "steer").await.unwrap(),
        SteerPending::Steered { .. }
    ));
    runs.begin_quiesce().await;
    assert!(
        runs.release_direct_run(
            direct,
            RunOutcome::Failed {
                message: Some("failed".to_string())
            }
            .into()
        )
        .await
        .unwrap()
    );
    assert_eq!(
        store
            .get_managed_submission("s", "steer")
            .unwrap()
            .unwrap()
            .state,
        SubmissionState::Pending
    );
    assert!(observer.events.lock().unwrap().iter().any(|event| {
        matches!(
            event,
            ManagedEvent::QueueChanged { pending, .. }
                if pending.iter().any(|record| record.submission.id == "steer")
        )
    }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_submit_receipts_reload_their_exact_durable_record() {
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    store.create_session_with_id("s", None).unwrap();
    let runner = Arc::new(TestRunner::default());
    let release_a_run = runner.block("a");
    let observer = Arc::new(Recorder::default());
    let (accept_entered_tx, accept_entered_rx) = oneshot::channel();
    let (accept_release_tx, accept_release_rx) = std::sync::mpsc::channel();
    let managed_store = Arc::new(TransitionFaultStore::blocking_accept(
        store.clone(),
        "a",
        accept_entered_tx,
        accept_release_rx,
    ));
    let runs = ManagedRuns::new(managed_store, runner.clone(), observer);

    let a_runs = runs.clone();
    let submit_a = tokio::spawn(async move { a_runs.submit("s", submit("a")).await });
    accept_entered_rx.await.unwrap();

    // B reaches the scheduler while A's caller is still returning from its
    // durable accept. FIFO means B's kick claims A.
    let receipt_b = runs.submit("s", submit("b")).await.unwrap();
    assert!(matches!(
        receipt_b.disposition,
        SubmissionDisposition::Queued { .. }
    ));
    wait_for(|| runner.started() == ["a"]).await;

    accept_release_tx.send(()).unwrap();
    let receipt_a = submit_a.await.unwrap().unwrap();
    let active = store.active_managed_run("s").unwrap().unwrap();
    assert!(matches!(
        receipt_a.disposition,
        SubmissionDisposition::Started { ref run_id } if run_id == &active.run_id
    ));

    release_a_run.send(()).unwrap();
    wait_for(|| {
        store
            .get_managed_submission("s", "b")
            .unwrap()
            .is_some_and(|record| record.state == SubmissionState::Succeeded)
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn aborting_submit_after_durable_claim_cannot_strand_the_launch() {
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    store.create_session_with_id("s", None).unwrap();
    let runner = Arc::new(TestRunner::default());
    let observer = Arc::new(Recorder::default());
    let (claim_entered_tx, claim_entered_rx) = oneshot::channel();
    let (claim_release_tx, claim_release_rx) = std::sync::mpsc::channel();
    let managed_store = Arc::new(TransitionFaultStore::blocking_claim(
        store.clone(),
        claim_entered_tx,
        claim_release_rx,
    ));
    let runs = ManagedRuns::new(managed_store, runner.clone(), observer.clone());

    let caller_runs = runs.clone();
    let caller = tokio::spawn(async move { caller_runs.submit("s", submit("a")).await });
    claim_entered_rx.await.unwrap();
    assert_eq!(
        store
            .get_managed_submission("s", "a")
            .unwrap()
            .unwrap()
            .state,
        SubmissionState::Claimed
    );

    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    claim_release_tx.send(()).unwrap();
    wait_for(|| {
        store
            .get_managed_submission("s", "a")
            .unwrap()
            .is_some_and(|record| record.state == SubmissionState::Succeeded)
    })
    .await;
    assert_eq!(runner.started(), ["a"]);
    assert!(observer.events.lock().unwrap().iter().any(
        |event| matches!(event, ManagedEvent::RunStarted { run, .. } if run.submission_id == "a")
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn successor_cannot_start_before_prior_settlement_is_published() {
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    store.create_session_with_id("s", None).unwrap();
    let runner = Arc::new(TestRunner::default());
    let release_a = runner.block("a");
    let release_b = runner.block("b");
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let observer = Arc::new(BlockingLifecycleObserver {
        target: BlockLifecycle::RunSettled,
        target_id: "a".to_string(),
        entered: Mutex::new(Some(entered_tx)),
        release: Mutex::new(Some(release_rx)),
        events: Mutex::new(Vec::new()),
    });
    let runs = ManagedRuns::new(
        Arc::new(SqliteManagedStore::new(store.clone())),
        runner.clone(),
        observer.clone(),
    );

    runs.submit("s", submit("a")).await.unwrap();
    wait_for(|| runner.started() == ["a"]).await;
    runs.submit("s", submit("b")).await.unwrap();
    release_a.send(()).unwrap();
    entered_rx.await.unwrap();
    assert_eq!(
        store
            .get_managed_submission("s", "a")
            .unwrap()
            .unwrap()
            .state,
        SubmissionState::Succeeded,
        "the durable settlement precedes its lifecycle publication"
    );

    runs.wake("s");
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(
        runner.started(),
        ["a"],
        "a successor started while the prior RunSettled observer was blocked"
    );

    release_tx.send(()).unwrap();
    wait_for(|| runner.started() == ["a", "b"]).await;
    {
        let events = observer.events.lock().unwrap();
        let settled_a = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    ManagedEvent::RunSettled { run, .. } if run.submission_id == "a"
                )
            })
            .unwrap();
        let started_b = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    ManagedEvent::RunStarted { run, .. } if run.submission_id == "b"
                )
            })
            .unwrap();
        assert!(settled_a < started_b);
    }
    release_b.send(()).unwrap();
    wait_for(|| {
        store
            .get_managed_submission("s", "b")
            .unwrap()
            .is_some_and(|record| record.state == SubmissionState::Succeeded)
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retry_cannot_start_before_input_commit_failure_is_published() {
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    store.create_session_with_id("s", None).unwrap();
    let runner = Arc::new(TestRunner::default());
    runner.fail_commit_once("a");
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let observer = Arc::new(BlockingLifecycleObserver {
        target: BlockLifecycle::InputCommitFailed,
        target_id: "a".to_string(),
        entered: Mutex::new(Some(entered_tx)),
        release: Mutex::new(Some(release_rx)),
        events: Mutex::new(Vec::new()),
    });
    let runs = ManagedRuns::new(
        Arc::new(SqliteManagedStore::new(store.clone())),
        runner.clone(),
        observer.clone(),
    );

    runs.submit("s", submit("a")).await.unwrap();
    entered_rx.await.unwrap();
    assert_eq!(
        store
            .get_managed_submission("s", "a")
            .unwrap()
            .unwrap()
            .state,
        SubmissionState::Pending,
        "the durable requeue precedes its lifecycle publication"
    );

    let receipt = runs.submit("s", submit("b")).await.unwrap();
    assert!(matches!(
        receipt.disposition,
        SubmissionDisposition::Queued { .. }
    ));
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(
        runner.started().is_empty(),
        "the requeued submission restarted while InputCommitFailed was blocked"
    );

    release_tx.send(()).unwrap();
    wait_for(|| {
        observer.events.lock().unwrap().iter().any(|event| {
            matches!(
                event,
                ManagedEvent::Fault(fault) if fault.phase == ManagedPhase::CommitInput
            )
        })
    })
    .await;
    runs.wake("s");
    wait_for(|| runner.started().len() == 2).await;
    let events = observer.events.lock().unwrap();
    let failed_a = events
        .iter()
        .position(|event| {
            matches!(
                event,
                ManagedEvent::InputCommitFailed { run, .. } if run.submission_id == "a"
            )
        })
        .unwrap();
    let retry_started_a = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| {
            matches!(
                event,
                ManagedEvent::RunStarted { run, .. } if run.submission_id == "a"
            )
            .then_some(index)
        })
        .nth(1)
        .unwrap();
    assert!(failed_a < retry_started_a);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_start_cannot_overtake_managed_terminal_publication() {
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    store.create_session_with_id("s", None).unwrap();
    let runner = Arc::new(TestRunner::default());
    let release_a = runner.block("a");
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let observer = Arc::new(BlockingLifecycleObserver {
        target: BlockLifecycle::RunSettled,
        target_id: "a".to_string(),
        entered: Mutex::new(Some(entered_tx)),
        release: Mutex::new(Some(release_rx)),
        events: Mutex::new(Vec::new()),
    });
    let runs = ManagedRuns::new(
        Arc::new(SqliteManagedStore::new(store.clone())),
        runner.clone(),
        observer.clone(),
    );

    runs.submit("s", submit("a")).await.unwrap();
    wait_for(|| runner.started() == ["a"]).await;
    release_a.send(()).unwrap();
    entered_rx.await.unwrap();
    assert!(store.active_managed_run("s").unwrap().is_none());
    assert!(matches!(
        runs.try_acquire_direct_run("s", "direct").await.unwrap(),
        DirectRunAcquire::Held { .. }
    ));
    assert!(observer.events.lock().unwrap().iter().all(|event| {
        !matches!(
            event,
            ManagedEvent::DirectRunStarted { run } if run.run_id == "direct"
        )
    }));

    release_tx.send(()).unwrap();
    let direct = acquire_direct_eventually(&runs, "s", "direct").await;
    {
        let events = observer.events.lock().unwrap();
        let managed_settled = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    ManagedEvent::RunSettled { run, .. } if run.submission_id == "a"
                )
            })
            .unwrap();
        let direct_started = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    ManagedEvent::DirectRunStarted { run } if run.run_id == "direct"
                )
            })
            .unwrap();
        assert!(managed_settled < direct_started);
    }
    assert!(
        runs.release_direct_run(direct, RunOutcome::Completed.into())
            .await
            .unwrap()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn managed_start_cannot_overtake_direct_terminal_publication() {
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    store.create_session_with_id("s", None).unwrap();
    let runner = Arc::new(TestRunner::default());
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let observer = Arc::new(BlockingLifecycleObserver {
        target: BlockLifecycle::DirectRunSettled,
        target_id: "direct".to_string(),
        entered: Mutex::new(Some(entered_tx)),
        release: Mutex::new(Some(release_rx)),
        events: Mutex::new(Vec::new()),
    });
    let runs = ManagedRuns::new(
        Arc::new(SqliteManagedStore::new(store.clone())),
        runner.clone(),
        observer.clone(),
    );

    let direct = acquire_direct_eventually(&runs, "s", "direct").await;
    assert!(
        !runs.cancel_active("s"),
        "direct leases are not cancellable"
    );
    assert!(matches!(
        runs.submit("s", submit("a")).await.unwrap().disposition,
        SubmissionDisposition::Queued { .. }
    ));

    let release_runs = runs.clone();
    let release_task = tokio::spawn(async move {
        release_runs
            .release_direct_run(direct, RunOutcome::Completed.into())
            .await
    });
    entered_rx.await.unwrap();
    assert!(store.active_managed_run("s").unwrap().is_none());
    runs.wake("s");
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(
        runner.started().is_empty(),
        "managed work started while DirectRunSettled was blocked"
    );

    release_tx.send(()).unwrap();
    assert!(release_task.await.unwrap().unwrap());
    wait_for(|| runner.started() == ["a"]).await;
    {
        let events = observer.events.lock().unwrap();
        let direct_settled = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    ManagedEvent::DirectRunSettled { run, .. } if run.run_id == "direct"
                )
            })
            .unwrap();
        let managed_started = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    ManagedEvent::RunStarted { run, .. } if run.submission_id == "a"
                )
            })
            .unwrap();
        assert!(direct_settled < managed_started);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_direct_acquire_handoff_releases_and_drains_pending_work() {
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    store.create_session_with_id("s", None).unwrap();
    let runner = Arc::new(TestRunner::default());
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let observer = Arc::new(BlockingLifecycleObserver {
        target: BlockLifecycle::DirectRunStarted,
        target_id: "direct".to_string(),
        entered: Mutex::new(Some(entered_tx)),
        release: Mutex::new(Some(release_rx)),
        events: Mutex::new(Vec::new()),
    });
    let runs = ManagedRuns::new(
        Arc::new(SqliteManagedStore::new(store.clone())),
        runner.clone(),
        observer.clone(),
    );

    let acquire_runs = runs.clone();
    let acquire_task =
        tokio::spawn(async move { acquire_runs.try_acquire_direct_run("s", "direct").await });
    entered_rx.await.unwrap();
    let submit_runs = runs.clone();
    let submit_task = tokio::spawn(async move { submit_runs.submit("s", submit("a")).await });
    wait_for(|| {
        store
            .get_managed_submission("s", "a")
            .unwrap()
            .is_some_and(|record| record.state == SubmissionState::Pending)
    })
    .await;
    acquire_task.abort();
    assert!(acquire_task.await.unwrap_err().is_cancelled());
    release_tx.send(()).unwrap();
    submit_task.await.unwrap().unwrap();

    wait_for(|| runner.started() == ["a"]).await;
    wait_for(|| store.active_managed_run("s").unwrap().is_none()).await;
    let events = observer.events.lock().unwrap();
    let direct_cancelled = events
        .iter()
        .position(|event| {
            matches!(
                event,
                ManagedEvent::DirectRunSettled {
                    run,
                    outcome: RunOutcome::Cancelled,
                    ..
                } if run.run_id == "direct"
            )
        })
        .unwrap();
    let managed_started = events
        .iter()
        .position(|event| {
            matches!(
                event,
                ManagedEvent::RunStarted { run, .. } if run.submission_id == "a"
            )
        })
        .unwrap();
    assert!(direct_cancelled < managed_started);
}

#[tokio::test]
async fn quiesce_waits_for_direct_fences_without_cancelling_them() {
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    store.create_session_with_id("s", None).unwrap();
    let runs = service(
        store,
        Arc::new(TestRunner::default()),
        Arc::new(Recorder::default()),
    );
    let direct = acquire_direct_eventually(&runs, "s", "direct").await;
    assert!(!runs.cancel_active("s"));
    runs.begin_quiesce().await;

    let wait_runs = runs.clone();
    let (quiesced_tx, mut quiesced_rx) = oneshot::channel();
    tokio::spawn(async move {
        wait_runs.wait_quiesced().await;
        let _ = quiesced_tx.send(());
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(30), &mut quiesced_rx)
            .await
            .is_err(),
        "quiesce ignored an owned direct fence"
    );
    assert!(
        runs.release_direct_run(direct, RunOutcome::Completed.into())
            .await
            .unwrap()
    );
    tokio::time::timeout(Duration::from_secs(1), quiesced_rx)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        runs.try_acquire_direct_run("s", "later").await.unwrap(),
        DirectRunAcquire::Quiescing
    ));
}

#[tokio::test]
async fn observer_panic_cannot_prevent_execution_or_settlement() {
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    store.create_session_with_id("s", None).unwrap();
    let runner = Arc::new(TestRunner::default());
    let runs = ManagedRuns::new(
        Arc::new(SqliteManagedStore::new(store.clone())),
        runner.clone(),
        Arc::new(PanickingObserver),
    );

    runs.submit("s", submit("a")).await.unwrap();
    wait_for(|| {
        store
            .get_managed_submission("s", "a")
            .unwrap()
            .is_some_and(|record| record.state == SubmissionState::Succeeded)
    })
    .await;
    assert_eq!(runner.started(), ["a"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_queue_snapshots_are_observed_in_durable_order() {
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    store.create_session_with_id("s", None).unwrap();
    let runner = Arc::new(TestRunner::default());
    let release_a = runner.block("a");
    let _hold_c = runner.block("c");
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let observer = Arc::new(BlockingFirstQueueObserver {
        queue_calls: AtomicUsize::new(0),
        block_next: AtomicBool::new(false),
        first_entered: Mutex::new(Some(entered_tx)),
        release_first: Mutex::new(Some(release_rx)),
        snapshots: Mutex::new(Vec::new()),
    });
    let runs = ManagedRuns::new(
        Arc::new(SqliteManagedStore::new(store.clone())),
        runner.clone(),
        observer.clone(),
    );
    runs.submit("s", submit("a")).await.unwrap();
    wait_for(|| runner.started() == ["a"]).await;
    runs.submit("s", submit("b")).await.unwrap();
    runs.submit("s", submit("c")).await.unwrap();

    observer.snapshots.lock().unwrap().clear();
    observer.queue_calls.store(0, Ordering::SeqCst);
    observer.block_next.store(true, Ordering::SeqCst);
    let submit_runs = runs.clone();
    let submit_d = tokio::spawn(async move { submit_runs.submit("s", submit("d")).await });
    entered_rx.await.unwrap();

    let cancel_runs = runs.clone();
    let cancel_b = tokio::spawn(async move { cancel_runs.cancel_pending("s", "b").await });
    wait_for(|| {
        store
            .get_managed_submission("s", "b")
            .unwrap()
            .is_some_and(|record| record.state == SubmissionState::Cancelled)
    })
    .await;
    release_a.send(()).unwrap();
    wait_for(|| {
        store
            .active_managed_run("s")
            .unwrap()
            .is_some_and(|run| run.submission_id.as_deref() == Some("c"))
    })
    .await;
    assert_eq!(
        observer.queue_calls.load(Ordering::SeqCst),
        1,
        "a newer snapshot overtook the blocked publication"
    );

    release_tx.send(()).unwrap();
    assert!(cancel_b.await.unwrap().unwrap());
    let receipt = submit_d.await.unwrap().unwrap();
    assert!(matches!(
        receipt.disposition,
        SubmissionDisposition::Queued { .. }
    ));
    wait_for(|| runner.started() == ["a", "c"]).await;
    {
        let snapshots = observer.snapshots.lock().unwrap();
        assert_eq!(
            snapshots.first(),
            Some(&vec!["b".to_string(), "c".to_string(), "d".to_string()])
        );
        assert!(
            snapshots
                .iter()
                .skip(1)
                .all(|snapshot| snapshot == &["d".to_string()]),
            "a later snapshot regressed after the durable queue advanced"
        );
    }
    assert_eq!(
        runs.pending("s")
            .await
            .unwrap()
            .iter()
            .map(|record| record.submission.id.as_str())
            .collect::<Vec<_>>(),
        ["d"]
    );
    runs.quiesce().await;
}

#[tokio::test]
async fn post_accept_claim_failure_returns_an_acknowledgement_with_a_fault() {
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    store.create_session_with_id("s", None).unwrap();
    // Simulate a host payload schema that can no longer decode the oldest
    // durable row. The stock adapter validates before claim.
    store
        .enqueue_managed_submission("s", "old", "not-json")
        .unwrap();
    let runs = service(
        store.clone(),
        Arc::new(TestRunner::default()),
        Arc::new(Recorder::default()),
    );

    let receipt = runs.submit("s", submit("new")).await.unwrap();
    assert!(receipt.inserted);
    assert!(matches!(
        receipt.disposition,
        SubmissionDisposition::Queued { .. }
    ));
    assert_eq!(
        receipt.scheduling_fault.as_ref().map(|fault| fault.phase),
        Some(ac_managed::ManagedPhase::Claim)
    );
    assert_eq!(
        store
            .get_managed_submission("s", "new")
            .unwrap()
            .unwrap()
            .state,
        SubmissionState::Pending
    );
    assert!(store.active_managed_run("s").unwrap().is_none());
}

#[tokio::test]
async fn different_sessions_can_run_concurrently() {
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    store.create_session_with_id("left", None).unwrap();
    store.create_session_with_id("right", None).unwrap();
    let runner = Arc::new(TestRunner::default());
    let release_left = runner.block("a");
    let release_right = runner.block("b");
    let runs = service(store, runner.clone(), Arc::new(Recorder::default()));

    runs.submit("left", submit("a")).await.unwrap();
    runs.submit("right", submit("b")).await.unwrap();
    wait_for(|| runner.active.load(Ordering::SeqCst) == 2).await;
    assert_eq!(runner.max_active.load(Ordering::SeqCst), 2);
    release_left.send(()).unwrap();
    release_right.send(()).unwrap();
}

#[tokio::test]
async fn cancelled_and_failed_runs_both_release_and_continue_draining() {
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    store.create_session_with_id("s", None).unwrap();
    let runner = Arc::new(TestRunner::default());
    let release_first = runner.block("a");
    runner.outcome("a", RunOutcome::Cancelled);
    runner.outcome(
        "b",
        RunOutcome::Failed {
            message: Some("planned".to_string()),
        },
    );
    let runs = service(store.clone(), runner.clone(), Arc::new(Recorder::default()));

    runs.submit("s", submit("a")).await.unwrap();
    runs.submit("s", submit("b")).await.unwrap();
    runs.submit("s", submit("c")).await.unwrap();
    release_first.send(()).unwrap();
    wait_for(|| runner.started().len() == 3).await;
    wait_for(|| {
        store
            .get_managed_submission("s", "c")
            .unwrap()
            .is_some_and(|record| record.state == SubmissionState::Succeeded)
    })
    .await;
    assert_eq!(
        store
            .get_managed_submission("s", "a")
            .unwrap()
            .unwrap()
            .state,
        SubmissionState::Cancelled
    );
    assert_eq!(
        store
            .get_managed_submission("s", "b")
            .unwrap()
            .unwrap()
            .state,
        SubmissionState::Failed
    );
}

#[tokio::test]
async fn cancellation_during_input_commit_prevents_sampling_and_continues_draining() {
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    store.create_session_with_id("s", None).unwrap();
    let runner = Arc::new(TestRunner::default());
    let release_commit = runner.block_commit("a");
    let runs = service(store.clone(), runner.clone(), Arc::new(Recorder::default()));

    runs.submit("s", submit("a")).await.unwrap();
    runs.submit("s", submit("b")).await.unwrap();
    assert!(runs.cancel_active("s"));
    release_commit.send(()).unwrap();

    wait_for(|| {
        store
            .get_managed_submission("s", "b")
            .unwrap()
            .is_some_and(|record| record.state == SubmissionState::Succeeded)
    })
    .await;
    assert_eq!(
        store
            .get_managed_submission("s", "a")
            .unwrap()
            .unwrap()
            .state,
        SubmissionState::Cancelled
    );
    assert_eq!(runner.started(), ["b"]);
    assert!(!runs.cancel_active("s"));
}

#[tokio::test]
async fn quiesce_cancels_owned_work_and_leaves_pending_work_unclaimed() {
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    store.create_session_with_id("s", None).unwrap();
    let runner = Arc::new(TestRunner::default());
    let _keep_blocked = runner.block("a");
    let runs = service(store.clone(), runner.clone(), Arc::new(Recorder::default()));

    runs.submit("s", submit("a")).await.unwrap();
    runs.submit("s", submit("b")).await.unwrap();
    wait_for(|| runner.started() == ["a"]).await;
    runs.quiesce().await;

    assert_eq!(
        store
            .get_managed_submission("s", "a")
            .unwrap()
            .unwrap()
            .state,
        SubmissionState::Cancelled
    );
    assert_eq!(
        store
            .get_managed_submission("s", "b")
            .unwrap()
            .unwrap()
            .state,
        SubmissionState::Pending
    );
    runs.wake("s");
    tokio::task::yield_now().await;
    assert_eq!(runner.started(), ["a"]);
    assert!(matches!(
        runs.submit("s", submit("c")).await.unwrap().disposition,
        SubmissionDisposition::Queued { .. }
    ));
    assert_eq!(runner.started(), ["a"]);
}

#[tokio::test]
async fn input_commit_failure_requeues_loudly_without_a_hot_loop() {
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    store.create_session_with_id("s", None).unwrap();
    let runner = Arc::new(TestRunner::default());
    runner.fail_commit_once("a");
    let observer = Arc::new(Recorder::default());
    let runs = service(store.clone(), runner.clone(), observer.clone());

    runs.submit("s", submit("a")).await.unwrap();
    wait_for(|| {
        let pending = store
            .get_managed_submission("s", "a")
            .unwrap()
            .is_some_and(|record| record.state == SubmissionState::Pending);
        let published = observer
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, ManagedEvent::InputCommitFailed { .. }));
        pending && published
    })
    .await;
    assert!(
        runner.started().is_empty(),
        "sampling began after commit failure"
    );
    assert!(
        observer
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|event| { matches!(event, ManagedEvent::InputCommitFailed { .. }) })
    );

    runs.wake("s");
    wait_for(|| runner.started() == ["a"]).await;
    wait_for(|| {
        store
            .get_managed_submission("s", "a")
            .unwrap()
            .is_some_and(|record| record.state == SubmissionState::Succeeded)
    })
    .await;
}

#[tokio::test]
async fn cancelling_a_requeued_fifo_head_automatically_drains_its_successor() {
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    store.create_session_with_id("s", None).unwrap();
    let runner = Arc::new(TestRunner::default());
    runner.fail_commit_once("a");
    let release_commit = runner.block_commit("a");
    let observer = Arc::new(Recorder::default());
    let runs = service(store.clone(), runner.clone(), observer.clone());

    runs.submit("s", submit("a")).await.unwrap();
    runs.submit("s", submit("b")).await.unwrap();
    release_commit.send(()).unwrap();
    wait_for(|| {
        let pending = ["a", "b"].into_iter().all(|id| {
            store
                .get_managed_submission("s", id)
                .unwrap()
                .is_some_and(|record| record.state == SubmissionState::Pending)
        });
        let published = observer
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, ManagedEvent::InputCommitFailed { .. }));
        pending && published
    })
    .await;
    assert!(runner.started().is_empty());

    assert!(runs.cancel_pending("s", "a").await.unwrap());
    wait_for(|| {
        store
            .get_managed_submission("s", "b")
            .unwrap()
            .is_some_and(|record| record.state == SubmissionState::Succeeded)
    })
    .await;
    assert_eq!(runner.started(), ["b"]);
    assert_eq!(
        store
            .get_managed_submission("s", "a")
            .unwrap()
            .unwrap()
            .state,
        SubmissionState::Cancelled
    );
}

#[tokio::test]
async fn uncertain_requeue_keeps_claim_fenced_without_a_false_pending_lifecycle() {
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    store.create_session_with_id("s", None).unwrap();
    let runner = Arc::new(TestRunner::default());
    runner.fail_commit_once("a");
    let observer = Arc::new(Recorder::default());
    let managed_store = Arc::new(TransitionFaultStore::new(store.clone(), true, false));
    let runs = ManagedRuns::new(managed_store, runner.clone(), observer.clone());

    runs.submit("s", submit("a")).await.unwrap();
    wait_for(|| {
        observer.events.lock().unwrap().iter().any(|event| {
            matches!(
                event,
                ManagedEvent::Fault(fault) if fault.phase == ManagedPhase::CommitInput
            )
        })
    })
    .await;

    assert_eq!(
        store
            .get_managed_submission("s", "a")
            .unwrap()
            .unwrap()
            .state,
        SubmissionState::Pending,
        "the injected store committed the requeue before returning its fault"
    );
    assert!(
        observer.events.lock().unwrap().iter().all(|event| {
            !matches!(
                event,
                ManagedEvent::InputCommitFailed { submission, .. }
                    if submission.state == SubmissionState::Pending
            )
        }),
        "an uncertain requeue published a fabricated pending lifecycle record"
    );

    let receipt = runs.submit("s", submit("b")).await.unwrap();
    assert!(matches!(
        receipt.disposition,
        SubmissionDisposition::Queued { .. }
    ));
    assert!(runner.started().is_empty());
    assert!(runs.cancel_active("s"));
    assert_eq!(
        store
            .get_managed_submission("s", "b")
            .unwrap()
            .unwrap()
            .state,
        SubmissionState::Pending
    );
}

#[tokio::test]
async fn uncertain_settlement_keeps_an_effective_in_memory_claim_fence() {
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    store.create_session_with_id("s", None).unwrap();
    let runner = Arc::new(TestRunner::default());
    let observer = Arc::new(Recorder::default());
    let managed_store = Arc::new(TransitionFaultStore::new(store.clone(), false, true));
    let runs = ManagedRuns::new(managed_store, runner.clone(), observer.clone());

    runs.submit("s", submit("a")).await.unwrap();
    wait_for(|| {
        observer.events.lock().unwrap().iter().any(|event| {
            matches!(
                event,
                ManagedEvent::Fault(fault) if fault.phase == ManagedPhase::Settle
            )
        })
    })
    .await;
    assert_eq!(
        store
            .get_managed_submission("s", "a")
            .unwrap()
            .unwrap()
            .state,
        SubmissionState::Succeeded,
        "the injected store committed settlement before returning its fault"
    );
    assert!(store.active_managed_run("s").unwrap().is_none());

    let receipt = runs.submit("s", submit("b")).await.unwrap();
    assert!(matches!(
        receipt.disposition,
        SubmissionDisposition::Queued { .. }
    ));
    assert_eq!(runner.started(), ["a"]);
    assert!(runs.cancel_active("s"));
    assert_eq!(
        store
            .get_managed_submission("s", "b")
            .unwrap()
            .unwrap()
            .state,
        SubmissionState::Pending
    );
}

#[tokio::test]
async fn reopen_interrupts_started_work_requeues_precommit_and_drains_pending() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("managed.db");
    {
        let store = Arc::new(SqliteStore::open(&path).unwrap());
        store.create_session_with_id("running", None).unwrap();
        store.create_session_with_id("claimed", None).unwrap();
        let adapter = SqliteManagedStore::<String>::new(store);
        adapter.accept("running", &submit("active")).unwrap();
        adapter.accept("running", &submit("later")).unwrap();
        let ManagedClaim::Claimed { run, .. } = adapter.claim_next("running", "r-active").unwrap()
        else {
            panic!("running claim");
        };
        assert!(adapter.mark_input_committed(&run).unwrap());

        adapter.accept("claimed", &submit("precommit")).unwrap();
        assert!(matches!(
            adapter.claim_next("claimed", "r-precommit").unwrap(),
            ManagedClaim::Claimed { .. }
        ));
    }

    let store = Arc::new(SqliteStore::open(&path).unwrap());
    let runner = Arc::new(TestRunner::default());
    let runs = service(store.clone(), runner.clone(), Arc::new(Recorder::default()));
    let report = runs.recover().await.unwrap();
    assert_eq!(report.interrupted, 1);
    assert_eq!(report.requeued, 1);
    assert_eq!(report.pending_sessions, 2);
    wait_for(|| runner.started().len() == 2).await;
    let mut started = runner.started();
    started.sort();
    assert_eq!(started, ["later", "precommit"]);
    assert!(!started.contains(&"active".to_string()));
    assert_eq!(
        store
            .get_managed_submission("running", "active")
            .unwrap()
            .unwrap()
            .state,
        SubmissionState::Interrupted
    );
}

#[tokio::test]
async fn deferred_recovery_reconciles_without_claiming_until_explicit_wake() {
    let store = Arc::new(SqliteStore::open_in_memory().unwrap());
    store.create_session_with_id("s", None).unwrap();
    let adapter = SqliteManagedStore::<String>::new(store.clone());
    adapter.accept("s", &submit("a")).unwrap();
    let runner = Arc::new(TestRunner::default());
    let runs = service(store.clone(), runner.clone(), Arc::new(Recorder::default()));

    let report = runs.recover_deferred().await.unwrap();
    assert_eq!(report.pending_sessions, 1);
    assert_eq!(
        store
            .get_managed_submission("s", "a")
            .unwrap()
            .unwrap()
            .state,
        SubmissionState::Pending
    );
    assert!(runner.started().is_empty());

    runs.wake("s");
    wait_for(|| runner.started() == ["a"]).await;
}
