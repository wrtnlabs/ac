//! Application-free example client:
//!
//! `cargo run -p ac-managed --example managed_client`

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ac_managed::{
    ActiveRunRef, BoxError, DirectRunAcquire, ManagedEvent, ManagedObserver, ManagedRunner,
    ManagedRuns, PendingReorder, RunContext, RunOutcome, RunSettlement, SqliteManagedStore,
    SteerDelivery, SteerPending, Submission, SubmissionRecord, SubmissionState,
};
use ac_store::SqliteStore;
use futures::future::BoxFuture;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

struct Runner {
    order: Mutex<Vec<String>>,
    first: Mutex<Option<oneshot::Receiver<()>>>,
    steers: Mutex<Vec<(String, String)>>,
}

impl ManagedRunner<String> for Runner {
    fn commit_input(
        self: Arc<Self>,
        run: RunContext,
        _submission: SubmissionRecord<String>,
    ) -> BoxFuture<'static, Result<(), BoxError>> {
        Box::pin(async move {
            println!("input durable: {}", run.submission_id);
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
            if cancel.is_cancelled() {
                return RunOutcome::Cancelled.into();
            }
            let id = submission.submission.id;
            self.order.lock().unwrap().push(id.clone());
            let first = self.first.lock().unwrap().take();
            if let Some(first) = first {
                tokio::select! {
                    _ = cancel.cancelled() => return RunOutcome::Cancelled.into(),
                    _ = first => {}
                }
            }
            let committed_steer_ids = self
                .steers
                .lock()
                .unwrap()
                .iter()
                .filter_map(|(run_id, id)| (run_id == &run.run_id).then_some(id.clone()))
                .collect();
            println!("completed: {id}");
            RunSettlement::new(RunOutcome::Completed, committed_steer_ids)
        })
    }

    fn steer(
        self: Arc<Self>,
        run: ActiveRunRef,
        submission: SubmissionRecord<String>,
    ) -> BoxFuture<'static, Result<SteerDelivery, BoxError>> {
        Box::pin(async move {
            let id = submission.submission.id;
            self.steers.lock().unwrap().push((run.run_id, id.clone()));
            println!("steer accepted: {id}");
            Ok(SteerDelivery::Accepted)
        })
    }
}

struct Observer;

impl ManagedObserver<String> for Observer {
    fn observe(&self, event: &ManagedEvent<String>) {
        match event {
            ManagedEvent::QueueChanged { pending, .. } => {
                let ids: Vec<_> = pending
                    .iter()
                    .map(|item| item.submission.id.as_str())
                    .collect();
                println!("pending: {ids:?}");
            }
            ManagedEvent::RunStarted { run, .. } => {
                println!("started: {}", run.submission_id);
            }
            ManagedEvent::RunSettled { run, steered, .. } => {
                let ids: Vec<_> = steered
                    .iter()
                    .map(|item| item.submission.id.as_str())
                    .collect();
                println!("settled: {} (steered: {ids:?})", run.submission_id);
            }
            ManagedEvent::DirectRunStarted { run } => {
                println!("direct started: {}", run.run_id);
            }
            ManagedEvent::DirectRunSettled { run, outcome, .. } => {
                println!("direct settled: {} ({outcome:?})", run.run_id);
            }
            _ => {}
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    let store = Arc::new(SqliteStore::open_in_memory()?);
    store.create_session_with_id("example", Some("managed example"))?;
    let (release_first, first) = oneshot::channel();
    let runner = Arc::new(Runner {
        order: Mutex::new(Vec::new()),
        first: Mutex::new(Some(first)),
        steers: Mutex::new(Vec::new()),
    });
    let runs = ManagedRuns::new(
        Arc::new(SqliteManagedStore::new(store.clone())),
        runner.clone(),
        Arc::new(Observer),
    );

    for id in ["first", "second", "third", "cancel-me", "steer-me"] {
        let receipt = runs
            .submit(
                "example",
                Submission {
                    id: id.to_string(),
                    payload: format!("opaque payload for {id}"),
                },
            )
            .await?;
        println!("accepted {id}: {:?}", receipt.disposition);
    }

    assert!(runs.cancel_pending("example", "cancel-me").await?);
    assert_eq!(
        store
            .get_managed_submission("example", "cancel-me")?
            .expect("cancelled record")
            .state,
        SubmissionState::Cancelled
    );
    println!("cancelled pending: cancel-me");

    let expected = ["second", "third", "steer-me"].map(str::to_string);
    let desired = ["third", "second", "steer-me"].map(str::to_string);
    assert_eq!(
        runs.reorder_pending("example", expected.to_vec(), desired.to_vec())
            .await?,
        PendingReorder::Reordered
    );
    assert!(matches!(
        runs.steer_pending("example", "steer-me").await?,
        SteerPending::Steered { .. }
    ));
    let _ = release_first.send(());

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if ["first", "second", "third"].into_iter().all(|id| {
                store
                    .get_managed_submission("example", id)
                    .ok()
                    .flatten()
                    .is_some_and(|record| record.state == SubmissionState::Succeeded)
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await?;
    assert_eq!(*runner.order.lock().unwrap(), ["first", "third", "second"]);
    assert_eq!(
        store
            .get_managed_submission("example", "steer-me")?
            .expect("steered record")
            .state,
        SubmissionState::Steered
    );

    let direct = match runs
        .try_acquire_direct_run("example", "direct-maintenance")
        .await?
    {
        DirectRunAcquire::Acquired(run) => run,
        other => panic!("idle direct run was not acquired: {other:?}"),
    };
    assert!(
        runs.release_direct_run(direct, RunOutcome::Completed.into())
            .await?
    );
    Ok(())
}
