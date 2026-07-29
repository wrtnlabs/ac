use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex, Weak};

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex as AsyncMutex, Notify, RwLock, oneshot};
use tokio_util::sync::CancellationToken;

use crate::SubmissionState;
use crate::sqlite::{
    ManagedClaim, ManagedRecovery, ManagedStore, StoreAccept, StoreDirectRunAcquire,
    StorePendingReorder, StoreSteerDelivery, StoreSteerReservation,
};

pub type BoxError = Box<dyn Error + Send + Sync + 'static>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Submission<P> {
    pub id: String,
    pub payload: P,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmissionRecord<P> {
    pub session_id: String,
    /// Immutable per-session acceptance order.
    pub sequence: u64,
    /// Mutable durable order among pending submissions.
    pub queue_position: u64,
    pub submission: Submission<P>,
    pub state: SubmissionState,
    pub run_id: Option<String>,
    pub accepted_at_ms: i64,
    pub started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunContext {
    pub session_id: String,
    pub run_id: String,
    pub submission_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveRunRef {
    pub session_id: String,
    pub run_id: String,
}

/// One direct (non-submission) run holding the same durable per-session lease
/// as managed queued work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectRunLease {
    pub session_id: String,
    pub run_id: String,
    pub started_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectRunAcquire {
    Acquired(DirectRunLease),
    Held { run_id: String },
    Quiescing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    Completed,
    Cancelled,
    Failed { message: Option<String> },
}

/// The terminal outcome plus an ordered proof of which delivered steers the
/// host durably committed into the run's own input history.
///
/// A host MUST list only submission ids it persisted before returning this
/// value. AC validates that each id is a `delivered` child of this exact run,
/// commits those children to `steered` in the given order, and restores every
/// unlisted reservation to `pending`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSettlement {
    pub outcome: RunOutcome,
    pub committed_steer_ids: Vec<String>,
}

impl RunSettlement {
    pub fn new(outcome: RunOutcome, committed_steer_ids: Vec<String>) -> Self {
        Self {
            outcome,
            committed_steer_ids,
        }
    }

    pub fn without_steers(outcome: RunOutcome) -> Self {
        Self::new(outcome, Vec::new())
    }
}

impl From<RunOutcome> for RunSettlement {
    fn from(outcome: RunOutcome) -> Self {
        Self::without_steers(outcome)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmissionDisposition {
    Started { run_id: String },
    Queued { position: u64 },
    Existing { state: SubmissionState },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingReorder {
    Reordered,
    Unchanged,
    Conflict { current_order: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SteerDelivery {
    Accepted,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SteerPending {
    Steered { run_id: String },
    AlreadySteered { run_id: String },
    NoActiveRun,
    NotFound,
    NotPending { state: SubmissionState },
    Unavailable { run_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptReceipt {
    pub submission_id: String,
    pub inserted: bool,
    pub sequence: u64,
    pub disposition: SubmissionDisposition,
    /// A post-accept claim/launch failure. The submission is already durable
    /// and remains eligible for a later wake; callers MUST treat this receipt
    /// as acknowledgement, not retry the transport blindly.
    pub scheduling_fault: Option<ManagedFault>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedPhase {
    Accept,
    Snapshot,
    CancelPending,
    ReorderPending,
    SteerPending,
    Claim,
    AcquireDirect,
    ReleaseDirect,
    CommitInput,
    MarkInputCommitted,
    Run,
    Requeue,
    Settle,
    Recover,
}

impl fmt::Display for ManagedPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Accept => "accept",
            Self::Snapshot => "snapshot",
            Self::CancelPending => "cancel-pending",
            Self::ReorderPending => "reorder-pending",
            Self::SteerPending => "steer-pending",
            Self::Claim => "claim",
            Self::AcquireDirect => "acquire-direct",
            Self::ReleaseDirect => "release-direct",
            Self::CommitInput => "commit-input",
            Self::MarkInputCommitted => "mark-input-committed",
            Self::Run => "run",
            Self::Requeue => "requeue",
            Self::Settle => "settle",
            Self::Recover => "recover",
        };
        f.write_str(value)
    }
}

#[derive(Debug, thiserror::Error)]
#[error(
    "managed {phase} failed for session {session_id}{submission_suffix}: {source}",
    submission_suffix = submission_suffix(.submission_id.as_deref())
)]
pub struct ManagedError {
    pub phase: ManagedPhase,
    pub session_id: String,
    pub submission_id: Option<String>,
    #[source]
    pub source: BoxError,
}

fn submission_suffix(id: Option<&str>) -> String {
    id.map_or_else(String::new, |id| format!(", submission {id}"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedFault {
    pub phase: ManagedPhase,
    pub session_id: String,
    pub submission_id: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryKind {
    Requeued,
    Interrupted,
    Released,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagedEvent<P> {
    QueueChanged {
        session_id: String,
        pending: Arc<[SubmissionRecord<P>]>,
    },
    RunStarted {
        run: RunContext,
        submission: SubmissionRecord<P>,
    },
    InputCommitted {
        run: RunContext,
        submission: SubmissionRecord<P>,
    },
    InputCommitFailed {
        run: RunContext,
        submission: SubmissionRecord<P>,
        message: String,
    },
    /// The active runtime accepted this pending submission and AC durably
    /// confirmed the submission as `delivered`.
    ///
    /// This is provisional runtime ownership, not proof that the input reached
    /// the run's committed history. [`ManagedEvent::RunSettled`] or
    /// [`ManagedEvent::DirectRunSettled`] remains the authoritative
    /// settlement event.
    SteerDelivered {
        run: ActiveRunRef,
        submission: SubmissionRecord<P>,
    },
    RunSettled {
        run: RunContext,
        submission: SubmissionRecord<P>,
        outcome: RunOutcome,
        /// Exact delivered submissions proven committed by the runner,
        /// preserving the runner's acknowledgement order.
        steered: Arc<[SubmissionRecord<P>]>,
    },
    DirectRunStarted {
        run: DirectRunLease,
    },
    DirectRunSettled {
        run: DirectRunLease,
        outcome: RunOutcome,
        /// Exact delivered submissions proven committed by the host,
        /// preserving the host's acknowledgement order.
        steered: Arc<[SubmissionRecord<P>]>,
    },
    Recovered {
        session_id: String,
        run_id: String,
        submission: Option<SubmissionRecord<P>>,
        kind: RecoveryKind,
    },
    Fault(ManagedFault),
}

/// Product/session adapter. `commit_input` is a separately named durability
/// phase and MUST be idempotent by `(session_id, submission_id)`.
pub trait ManagedRunner<P>: Send + Sync + 'static {
    fn commit_input(
        self: Arc<Self>,
        run: RunContext,
        submission: SubmissionRecord<P>,
    ) -> BoxFuture<'static, Result<(), BoxError>>;

    /// Resolves only after the run's own persistence hooks have completed.
    /// The settlement must acknowledge exactly the delivered steer ids that
    /// those hooks durably committed, in persistence order.
    fn run(
        self: Arc<Self>,
        run: RunContext,
        submission: SubmissionRecord<P>,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, RunSettlement>;

    /// Deliver one durably reserved pending submission into the active run's
    /// steer queue. `Accepted` means the runtime has taken ownership; AC first
    /// advances the durable reservation to `delivered`, then keeps it until
    /// settlement proves whether the host persisted it. `Unavailable` or
    /// `Err` MUST mean the runtime did not take ownership; AC then restores
    /// the record to pending, so returning either after partial acceptance can
    /// duplicate input. The default lets schedulers without steering remain
    /// valid.
    fn steer(
        self: Arc<Self>,
        _run: ActiveRunRef,
        _submission: SubmissionRecord<P>,
    ) -> BoxFuture<'static, Result<SteerDelivery, BoxError>> {
        Box::pin(async { Ok(SteerDelivery::Unavailable) })
    }
}

pub trait ManagedObserver<P>: Send + Sync + 'static {
    fn observe(&self, event: &ManagedEvent<P>);
}

pub struct NullObserver;

impl<P> ManagedObserver<P> for NullObserver {
    fn observe(&self, _event: &ManagedEvent<P>) {}
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RecoveryReport {
    pub requeued: usize,
    pub interrupted: usize,
    pub released: usize,
    pub pending_sessions: usize,
}

/// Durable per-session sequential scheduler for submission-backed and direct
/// runs.
pub struct ManagedRuns<P> {
    store: Arc<dyn ManagedStore<P>>,
    runner: Arc<dyn ManagedRunner<P>>,
    observer: Arc<dyn ManagedObserver<P>>,
    gates: Mutex<HashMap<String, Weak<AsyncMutex<()>>>>,
    queue_gates: Mutex<HashMap<String, Weak<AsyncMutex<()>>>>,
    active: Mutex<HashMap<String, ActiveRun>>,
    lifecycle: RwLock<ManagedLifecycle>,
    active_changed: Notify,
}

#[derive(Clone)]
struct ActiveRun {
    run_id: String,
    kind: ActiveRunKind,
}

#[derive(Clone)]
enum ActiveRunKind {
    Managed { cancel: CancellationToken },
    Direct,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ManagedLifecycle {
    Running,
    Quiescing,
}

impl<P> ManagedRuns<P>
where
    P: Clone + Send + Sync + 'static,
{
    pub fn new(
        store: Arc<dyn ManagedStore<P>>,
        runner: Arc<dyn ManagedRunner<P>>,
        observer: Arc<dyn ManagedObserver<P>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            runner,
            observer,
            gates: Mutex::new(HashMap::new()),
            queue_gates: Mutex::new(HashMap::new()),
            active: Mutex::new(HashMap::new()),
            lifecycle: RwLock::new(ManagedLifecycle::Running),
            active_changed: Notify::new(),
        })
    }

    /// Accept one submission durably and immediately make a backend-side
    /// start-or-queue decision.
    pub async fn submit(
        self: &Arc<Self>,
        session_id: impl Into<String>,
        submission: Submission<P>,
    ) -> Result<AcceptReceipt, ManagedError> {
        let session_id = session_id.into();
        let submission_id = submission.id.clone();
        let task_session_id = session_id.clone();
        let this = self.clone();
        // The whole accept-to-launch handshake is detached before its first
        // await. Dropping a client request then drops only this JoinHandle; it
        // cannot cancel a blocking accept/claim after the durable transition
        // has committed and strand the lease before registration.
        tokio::spawn(async move { this.submit_inner(task_session_id, submission).await })
            .await
            .map_err(|error| ManagedError {
                phase: ManagedPhase::Accept,
                session_id,
                submission_id: Some(submission_id),
                source: Box::new(error),
            })?
    }

    async fn submit_inner(
        self: Arc<Self>,
        session_id: String,
        submission: Submission<P>,
    ) -> Result<AcceptReceipt, ManagedError> {
        let submission_id = submission.id.clone();
        let store_session_id = session_id.clone();
        let accepted: StoreAccept<P> = self
            .store_call(
                ManagedPhase::Accept,
                &session_id,
                Some(&submission_id),
                move |store| store.accept(&store_session_id, &submission),
            )
            .await?;

        self.emit_queue_changed_best_effort(&accepted.record.session_id)
            .await;
        let mut scheduling_fault = match self.kick_once(&accepted.record.session_id).await {
            Ok(_) => None,
            Err(error) => {
                let fault = self.fault(&error);
                self.observe(ManagedEvent::Fault(fault.clone()));
                Some(fault)
            }
        };

        // Another same-session submit may have claimed this record. Reload the
        // exact durable row instead of deriving the receipt from the stale
        // accept snapshot or from which caller happened to win the drain gate.
        let lookup_session_id = accepted.record.session_id.clone();
        let lookup_submission_id = accepted.record.submission.id.clone();
        let current = self
            .store_call(
                ManagedPhase::Snapshot,
                &accepted.record.session_id,
                Some(&accepted.record.submission.id),
                move |store| store.get(&lookup_session_id, &lookup_submission_id),
            )
            .await;
        let record = match current {
            Ok(Some(record)) => record,
            Ok(None) => {
                let error = ManagedError {
                    phase: ManagedPhase::Snapshot,
                    session_id: accepted.record.session_id.clone(),
                    submission_id: Some(accepted.record.submission.id.clone()),
                    source: Box::new(InvariantError(
                        "accepted submission disappeared before receipt projection".to_string(),
                    )),
                };
                let fault = self.fault(&error);
                self.observe(ManagedEvent::Fault(fault.clone()));
                if scheduling_fault.is_none() {
                    scheduling_fault = Some(fault);
                }
                accepted.record.clone()
            }
            Err(error) => {
                let fault = self.fault(&error);
                self.observe(ManagedEvent::Fault(fault.clone()));
                if scheduling_fault.is_none() {
                    scheduling_fault = Some(fault);
                }
                accepted.record.clone()
            }
        };
        let disposition = match (record.state, record.run_id.as_ref()) {
            (SubmissionState::Pending, _) => SubmissionDisposition::Queued {
                position: record.queue_position,
            },
            (SubmissionState::Claimed | SubmissionState::Running, Some(run_id)) => {
                SubmissionDisposition::Started {
                    run_id: run_id.clone(),
                }
            }
            (state, _) => SubmissionDisposition::Existing { state },
        };
        Ok(AcceptReceipt {
            submission_id: record.submission.id,
            inserted: accepted.inserted,
            sequence: record.sequence,
            disposition,
            scheduling_fault,
        })
    }

    pub async fn pending(
        &self,
        session_id: impl Into<String>,
    ) -> Result<Vec<SubmissionRecord<P>>, ManagedError> {
        let session_id = session_id.into();
        let store_session_id = session_id.clone();
        self.store_call(ManagedPhase::Snapshot, &session_id, None, move |store| {
            store.pending(&store_session_id)
        })
        .await
    }

    /// Load one exact durable submission record.
    pub async fn get(
        &self,
        session_id: impl Into<String>,
        submission_id: impl Into<String>,
    ) -> Result<Option<SubmissionRecord<P>>, ManagedError> {
        let session_id = session_id.into();
        let submission_id = submission_id.into();
        let store_session_id = session_id.clone();
        let store_submission_id = submission_id.clone();
        self.store_call(
            ManagedPhase::Snapshot,
            &session_id,
            Some(&submission_id),
            move |store| store.get(&store_session_id, &store_submission_id),
        )
        .await
    }

    /// Compare-and-swap the complete pending order.
    ///
    /// Both slices name the same unique submissions. `expected_order` is the
    /// caller's last authoritative snapshot; a concurrent queue mutation is
    /// reported as [`PendingReorder::Conflict`] instead of being overwritten.
    pub async fn reorder_pending(
        self: &Arc<Self>,
        session_id: impl Into<String>,
        expected_order: Vec<String>,
        desired_order: Vec<String>,
    ) -> Result<PendingReorder, ManagedError> {
        let session_id = session_id.into();
        let error_session_id = session_id.clone();
        let this = self.clone();
        tokio::spawn(async move {
            this.reorder_pending_inner(session_id, expected_order, desired_order)
                .await
        })
        .await
        .map_err(|error| ManagedError {
            phase: ManagedPhase::ReorderPending,
            session_id: error_session_id,
            submission_id: None,
            source: Box::new(error),
        })?
    }

    async fn reorder_pending_inner(
        self: &Arc<Self>,
        session_id: String,
        expected_order: Vec<String>,
        desired_order: Vec<String>,
    ) -> Result<PendingReorder, ManagedError> {
        let gate = self.gate(&session_id);
        let _pass = gate.lock().await;
        let store_session_id = session_id.clone();
        let reordered = self
            .store_call(
                ManagedPhase::ReorderPending,
                &session_id,
                None,
                move |store| {
                    store.reorder_pending(&store_session_id, &expected_order, &desired_order)
                },
            )
            .await?;
        let result = match reordered {
            StorePendingReorder::Reordered => PendingReorder::Reordered,
            StorePendingReorder::Unchanged => PendingReorder::Unchanged,
            StorePendingReorder::Conflict { current_order } => {
                PendingReorder::Conflict { current_order }
            }
        };
        self.emit_queue_changed_best_effort(&session_id).await;
        if result == PendingReorder::Reordered {
            self.wake(session_id);
        }
        Ok(result)
    }

    /// Losslessly promote one pending submission into the active run's steer
    /// queue.
    ///
    /// AC first commits a durable `steering` reservation bound to the exact
    /// active run, then asks the runner to deliver it. Caller cancellation
    /// cannot split those phases. A rejected delivery is restored to pending;
    /// accepted delivery is durably marked `delivered` and remains bound until
    /// active-run settlement acknowledges or restores it.
    pub async fn steer_pending(
        self: &Arc<Self>,
        session_id: impl Into<String>,
        submission_id: impl Into<String>,
    ) -> Result<SteerPending, ManagedError> {
        let session_id = session_id.into();
        let submission_id = submission_id.into();
        let error_session_id = session_id.clone();
        let error_submission_id = submission_id.clone();
        let this = self.clone();
        tokio::spawn(async move { this.steer_pending_inner(session_id, submission_id).await })
            .await
            .map_err(|error| ManagedError {
                phase: ManagedPhase::SteerPending,
                session_id: error_session_id,
                submission_id: Some(error_submission_id),
                source: Box::new(error),
            })?
    }

    async fn steer_pending_inner(
        self: &Arc<Self>,
        session_id: String,
        submission_id: String,
    ) -> Result<SteerPending, ManagedError> {
        let gate = self.gate(&session_id);
        let _pass = gate.lock().await;
        let store_session_id = session_id.clone();
        let store_submission_id = submission_id.clone();
        let record = self
            .store_call(
                ManagedPhase::SteerPending,
                &session_id,
                Some(&submission_id),
                move |store| store.get(&store_session_id, &store_submission_id),
            )
            .await?;
        let Some(record) = record else {
            return Ok(SteerPending::NotFound);
        };
        match record.state {
            SubmissionState::Steering | SubmissionState::Delivered | SubmissionState::Steered => {
                let run_id = record.run_id.ok_or_else(|| ManagedError {
                    phase: ManagedPhase::SteerPending,
                    session_id: session_id.clone(),
                    submission_id: Some(submission_id.clone()),
                    source: Box::new(InvariantError(
                        "steering submission has no bound run id".to_string(),
                    )),
                })?;
                return Ok(SteerPending::AlreadySteered { run_id });
            }
            SubmissionState::Pending => {}
            state => return Ok(SteerPending::NotPending { state }),
        }

        let Some(active) = self
            .active
            .lock()
            .expect("managed active-run map poisoned")
            .get(&session_id)
            .cloned()
        else {
            return Ok(SteerPending::NoActiveRun);
        };
        let active_ref = ActiveRunRef {
            session_id: session_id.clone(),
            run_id: active.run_id.clone(),
        };
        let store_session_id = session_id.clone();
        let store_submission_id = submission_id.clone();
        let store_run_id = active.run_id.clone();
        let reservation = self
            .store_call(
                ManagedPhase::SteerPending,
                &session_id,
                Some(&submission_id),
                move |store| {
                    store.begin_steer(&store_session_id, &store_submission_id, &store_run_id)
                },
            )
            .await?;
        let reserved = match reservation {
            StoreSteerReservation::Begun(record) => record,
            StoreSteerReservation::AlreadySteering(record)
            | StoreSteerReservation::AlreadySteered(record) => {
                let run_id = record.run_id.ok_or_else(|| ManagedError {
                    phase: ManagedPhase::SteerPending,
                    session_id: session_id.clone(),
                    submission_id: Some(submission_id.clone()),
                    source: Box::new(InvariantError(
                        "steering submission has no bound run id".to_string(),
                    )),
                })?;
                return Ok(SteerPending::AlreadySteered { run_id });
            }
            StoreSteerReservation::NotPending(record) => {
                return Ok(SteerPending::NotPending {
                    state: record.state,
                });
            }
            StoreSteerReservation::Missing => return Ok(SteerPending::NotFound),
            StoreSteerReservation::RunMismatch {
                active_run_id: None,
            } => return Ok(SteerPending::NoActiveRun),
            StoreSteerReservation::RunMismatch { active_run_id } => {
                return Err(ManagedError {
                    phase: ManagedPhase::SteerPending,
                    session_id,
                    submission_id: Some(submission_id),
                    source: Box::new(InvariantError(format!(
                        "active-run fence mismatch while reserving steer: {active_run_id:?}"
                    ))),
                });
            }
        };
        self.emit_queue_changed_best_effort(&session_id).await;

        let delivery = tokio::spawn(self.runner.clone().steer(active_ref.clone(), reserved)).await;
        match delivery {
            Ok(Ok(SteerDelivery::Accepted)) => {
                // Runtime ownership is irreversible: after `Accepted`, AC may
                // neither roll the record back nor release the session while
                // it is still only `steering`. Keep the session gate and retry
                // the failure-atomic confirmation until the durable record
                // converges to `delivered`. A persistent store failure
                // intentionally retains the active fence; process recovery is
                // then the conservative at-least-once escape hatch.
                let mut retry_delay = std::time::Duration::from_millis(10);
                let mut fault_reported = false;
                let delivered = loop {
                    let store_session_id = session_id.clone();
                    let store_submission_id = submission_id.clone();
                    let store_run_id = active_ref.run_id.clone();
                    match self
                        .store_call(
                            ManagedPhase::SteerPending,
                            &session_id,
                            Some(&submission_id),
                            move |store| {
                                store.mark_steer_delivered(
                                    &store_session_id,
                                    &store_submission_id,
                                    &store_run_id,
                                )
                            },
                        )
                        .await
                    {
                        Ok(delivered) => break delivered,
                        Err(error) => {
                            if !fault_reported {
                                self.emit_fault(&error);
                                fault_reported = true;
                            }
                            tokio::time::sleep(retry_delay).await;
                            retry_delay = (retry_delay * 2).min(std::time::Duration::from_secs(5));
                        }
                    }
                };
                match delivered {
                    StoreSteerDelivery::Delivered(submission)
                    | StoreSteerDelivery::AlreadyDelivered(submission) => {
                        self.observe(ManagedEvent::SteerDelivered {
                            run: active_ref.clone(),
                            submission,
                        });
                        Ok(SteerPending::Steered {
                            run_id: active_ref.run_id,
                        })
                    }
                    StoreSteerDelivery::AlreadySteered(_) => Ok(SteerPending::Steered {
                        run_id: active_ref.run_id,
                    }),
                    StoreSteerDelivery::NotSteering(record) => Err(ManagedError {
                        phase: ManagedPhase::SteerPending,
                        session_id,
                        submission_id: Some(submission_id),
                        source: Box::new(InvariantError(format!(
                            "accepted steer changed to {:?} before delivery confirmation",
                            record.state
                        ))),
                    }),
                    StoreSteerDelivery::Missing => Err(ManagedError {
                        phase: ManagedPhase::SteerPending,
                        session_id,
                        submission_id: Some(submission_id),
                        source: Box::new(InvariantError(
                            "accepted steer disappeared before delivery confirmation".to_string(),
                        )),
                    }),
                    StoreSteerDelivery::RunMismatch { active_run_id } => Err(ManagedError {
                        phase: ManagedPhase::SteerPending,
                        session_id,
                        submission_id: Some(submission_id),
                        source: Box::new(InvariantError(format!(
                            "active-run fence mismatch after accepting steer: {active_run_id:?}"
                        ))),
                    }),
                }
            }
            Ok(Ok(SteerDelivery::Unavailable)) => {
                self.rollback_steer(
                    &session_id,
                    &submission_id,
                    &active_ref.run_id,
                    "active run could not accept steer",
                )
                .await?;
                Ok(SteerPending::Unavailable {
                    run_id: active_ref.run_id,
                })
            }
            Ok(Err(error)) => {
                let message = error.to_string();
                self.rollback_steer(&session_id, &submission_id, &active_ref.run_id, &message)
                    .await?;
                Err(ManagedError {
                    phase: ManagedPhase::SteerPending,
                    session_id,
                    submission_id: Some(submission_id),
                    source: error,
                })
            }
            Err(error) => {
                let message = format!("managed steer task failed: {error}");
                self.rollback_steer(&session_id, &submission_id, &active_ref.run_id, &message)
                    .await?;
                Err(ManagedError {
                    phase: ManagedPhase::SteerPending,
                    session_id,
                    submission_id: Some(submission_id),
                    source: Box::new(error),
                })
            }
        }
    }

    async fn rollback_steer(
        &self,
        session_id: &str,
        submission_id: &str,
        run_id: &str,
        message: &str,
    ) -> Result<(), ManagedError> {
        let store_session_id = session_id.to_string();
        let store_submission_id = submission_id.to_string();
        let store_run_id = run_id.to_string();
        let store_message = message.to_string();
        let rolled_back = self
            .store_call(
                ManagedPhase::SteerPending,
                session_id,
                Some(submission_id),
                move |store| {
                    store.rollback_steer(
                        &store_session_id,
                        &store_submission_id,
                        &store_run_id,
                        &store_message,
                    )
                },
            )
            .await?;
        if !rolled_back {
            return Err(ManagedError {
                phase: ManagedPhase::SteerPending,
                session_id: session_id.to_string(),
                submission_id: Some(submission_id.to_string()),
                source: Box::new(InvariantError(
                    "steer reservation could not be restored to pending".to_string(),
                )),
            });
        }
        self.emit_queue_changed_best_effort(session_id).await;
        Ok(())
    }

    /// Acquire a direct (non-submission) run under the same per-session
    /// single-flight and lifecycle publication fence as queued work.
    ///
    /// The returned lease is handed off cancellation-safely: if the caller is
    /// dropped before receiving it, AC releases it as cancelled and wakes
    /// pending work.
    pub async fn try_acquire_direct_run(
        self: &Arc<Self>,
        session_id: impl Into<String>,
        run_id: impl Into<String>,
    ) -> Result<DirectRunAcquire, ManagedError> {
        let session_id = session_id.into();
        let run_id = run_id.into();
        let error_session_id = session_id.clone();
        let this = self.clone();
        let (response_tx, response_rx) = oneshot::channel();
        tokio::spawn(async move {
            match this.try_acquire_direct_run_inner(session_id, run_id).await {
                Ok(DirectRunAcquire::Acquired(run)) => {
                    let (handoff_tx, handoff_rx) = oneshot::channel();
                    let sent = response_tx.send(Ok((
                        DirectRunAcquire::Acquired(run.clone()),
                        Some(handoff_tx),
                    )));
                    if (sent.is_err() || handoff_rx.await.is_err())
                        && let Err(error) = this
                            .release_direct_run_inner(
                                run,
                                RunSettlement::from(RunOutcome::Cancelled),
                            )
                            .await
                    {
                        this.emit_fault(&error);
                    }
                }
                Ok(acquire) => {
                    let _ = response_tx.send(Ok((acquire, None)));
                }
                Err(error) => {
                    let _ = response_tx.send(Err(error));
                }
            }
        });

        let (acquire, handoff) = response_rx.await.map_err(|_| ManagedError {
            phase: ManagedPhase::AcquireDirect,
            session_id: error_session_id,
            submission_id: None,
            source: Box::new(InvariantError(
                "direct-run acquisition task ended before handoff".to_string(),
            )),
        })??;
        if let Some(handoff) = handoff {
            let _ = handoff.send(());
        }
        Ok(acquire)
    }

    async fn try_acquire_direct_run_inner(
        self: &Arc<Self>,
        session_id: String,
        run_id: String,
    ) -> Result<DirectRunAcquire, ManagedError> {
        let gate = self.gate(&session_id);
        let _pass = gate.lock().await;
        let lifecycle = self.lifecycle.read().await;
        if *lifecycle == ManagedLifecycle::Quiescing {
            return Ok(DirectRunAcquire::Quiescing);
        }
        if let Some(active_run_id) = self
            .active
            .lock()
            .expect("managed active-run map poisoned")
            .get(&session_id)
            .map(|active| active.run_id.clone())
        {
            return Ok(DirectRunAcquire::Held {
                run_id: active_run_id,
            });
        }

        let store_session_id = session_id.clone();
        let store_run_id = run_id.clone();
        let acquired = self
            .store_call(
                ManagedPhase::AcquireDirect,
                &session_id,
                None,
                move |store| store.try_acquire_direct_run(&store_session_id, &store_run_id),
            )
            .await?;
        let run = match acquired {
            StoreDirectRunAcquire::Held { run_id } => {
                return Ok(DirectRunAcquire::Held { run_id });
            }
            StoreDirectRunAcquire::Acquired(run) => run,
        };
        let collision = {
            let mut active = self.active.lock().expect("managed active-run map poisoned");
            match active.entry(run.session_id.clone()) {
                Entry::Vacant(entry) => {
                    entry.insert(ActiveRun {
                        run_id: run.run_id.clone(),
                        kind: ActiveRunKind::Direct,
                    });
                    false
                }
                Entry::Occupied(_) => true,
            }
        };
        if collision {
            return Err(ManagedError {
                phase: ManagedPhase::AcquireDirect,
                session_id: run.session_id,
                submission_id: None,
                source: Box::new(InvariantError(
                    "direct-run lease collided with an in-memory active fence".to_string(),
                )),
            });
        }

        // As with managed claims, registration happens under the lifecycle
        // read gate. Quiesce may begin while projection is blocked, but it
        // will then wait for this direct fence to be explicitly released.
        drop(lifecycle);
        self.observe(ManagedEvent::DirectRunStarted { run: run.clone() });
        Ok(DirectRunAcquire::Acquired(run))
    }

    /// Guardedly release a direct run, publish its terminal outcome, clear its
    /// in-memory fence, and wake the next pending managed submission.
    ///
    /// The release task is detached before its first await, so caller
    /// cancellation cannot expose an idle session before terminal projection.
    pub async fn release_direct_run(
        self: &Arc<Self>,
        run: DirectRunLease,
        settlement: RunSettlement,
    ) -> Result<bool, ManagedError> {
        let error_session_id = run.session_id.clone();
        let this = self.clone();
        tokio::spawn(async move { this.release_direct_run_inner(run, settlement).await })
            .await
            .map_err(|error| ManagedError {
                phase: ManagedPhase::ReleaseDirect,
                session_id: error_session_id,
                submission_id: None,
                source: Box::new(error),
            })?
    }

    async fn release_direct_run_inner(
        self: &Arc<Self>,
        run: DirectRunLease,
        settlement: RunSettlement,
    ) -> Result<bool, ManagedError> {
        let gate = self.gate(&run.session_id);
        let _pass = gate.lock().await;
        let owned = self
            .active
            .lock()
            .expect("managed active-run map poisoned")
            .get(&run.session_id)
            .is_some_and(|active| {
                active.run_id == run.run_id && matches!(active.kind, ActiveRunKind::Direct)
            });
        if !owned {
            return Ok(false);
        }

        let store_run = run.clone();
        let store_settlement = settlement.clone();
        let released = self
            .store_call(
                ManagedPhase::ReleaseDirect,
                &run.session_id,
                None,
                move |store| store.release_direct_run(&store_run, &store_settlement),
            )
            .await;
        let stored = match released {
            Ok(stored) if stored.settled => stored,
            Ok(_) => {
                let error = ManagedError {
                    phase: ManagedPhase::ReleaseDirect,
                    session_id: run.session_id,
                    submission_id: None,
                    source: Box::new(InvariantError(
                        "owned direct-run lease could not be released".to_string(),
                    )),
                };
                self.emit_fault(&error);
                return Err(error);
            }
            Err(error) => {
                self.emit_fault(&error);
                return Err(error);
            }
        };

        self.observe(ManagedEvent::DirectRunSettled {
            run: run.clone(),
            outcome: settlement.outcome,
            steered: stored.steered.into(),
        });
        self.clear_active_id(&run.session_id, &run.run_id);
        self.wake(run.session_id.clone());
        self.emit_queue_changed_best_effort(&run.session_id).await;
        Ok(true)
    }

    pub async fn cancel_pending(
        self: &Arc<Self>,
        session_id: impl Into<String>,
        submission_id: impl Into<String>,
    ) -> Result<bool, ManagedError> {
        let session_id = session_id.into();
        let submission_id = submission_id.into();
        let store_session_id = session_id.clone();
        let store_submission_id = submission_id.clone();
        let changed = self
            .store_call(
                ManagedPhase::CancelPending,
                &session_id,
                Some(&submission_id),
                move |store| store.cancel_pending(&store_session_id, &store_submission_id),
            )
            .await?;
        if changed {
            self.emit_queue_changed_best_effort(&session_id).await;
            // Removing a blocked/requeued FIFO head is itself a scheduling
            // boundary. The backend, not the queue-editing client, owns
            // continuing any eligible successor.
            self.wake(session_id);
        }
        Ok(changed)
    }

    /// Request cancellation of the session's claimed/running managed work.
    ///
    /// The token exists before `RunStarted` is observed, so this closes the
    /// pre-runner race: a host can cancel while input commit is still in
    /// progress, and the runner receives an already-cancelled token before it
    /// begins sampling. Settlement remains the normal `Cancelled` path.
    pub fn cancel_active(&self, session_id: &str) -> bool {
        let active = self
            .active
            .lock()
            .expect("managed active-run map poisoned")
            .get(session_id)
            .cloned();
        let Some(ActiveRun {
            kind: ActiveRunKind::Managed { cancel },
            ..
        }) = active
        else {
            return false;
        };
        cancel.cancel();
        true
    }

    /// Wake a session after recovery or restoration of an external execution
    /// prerequisite. Ordinary `submit`, direct release, changed pending
    /// cancellation, and managed settlement wake automatically.
    pub fn wake(self: &Arc<Self>, session_id: impl Into<String>) {
        let this = self.clone();
        let session_id = session_id.into();
        tokio::spawn(async move {
            if let Err(error) = this.kick_once(&session_id).await {
                this.emit_fault(&error);
            }
        });
    }

    /// Permanently stop this scheduler instance from claiming more work and
    /// request cancellation of every submission-backed run it already owns.
    ///
    /// Durable pending submissions remain pending for the next scheduler
    /// instance. Call [`Self::wait_quiesced`] (normally under a host deadline)
    /// before dropping the runtime when settled terminal projection matters.
    pub async fn begin_quiesce(&self) {
        let mut lifecycle = self.lifecycle.write().await;
        *lifecycle = ManagedLifecycle::Quiescing;
        let cancels = self
            .active
            .lock()
            .expect("managed active-run map poisoned")
            .values()
            .filter_map(|active| match &active.kind {
                ActiveRunKind::Managed { cancel } => Some(cancel.clone()),
                ActiveRunKind::Direct => None,
            })
            .collect::<Vec<_>>();
        drop(lifecycle);
        for cancel in cancels {
            cancel.cancel();
        }
    }

    /// Wait until every run owned before or during [`Self::begin_quiesce`] has
    /// reached AC settlement. This does not wait for durable pending work.
    pub async fn wait_quiesced(&self) {
        loop {
            let changed = self.active_changed.notified();
            if self
                .active
                .lock()
                .expect("managed active-run map poisoned")
                .is_empty()
            {
                return;
            }
            changed.await;
        }
    }

    /// Convenience form of [`Self::begin_quiesce`] plus
    /// [`Self::wait_quiesced`].
    pub async fn quiesce(&self) {
        self.begin_quiesce().await;
        self.wait_quiesced().await;
    }

    /// Reconcile abandoned active state, then wake every session that still
    /// has durable pending work. Call once after the host has exclusive
    /// authority over the store.
    pub async fn recover(self: &Arc<Self>) -> Result<RecoveryReport, ManagedError> {
        self.recover_inner(true).await
    }

    /// Reconcile abandoned active state without claiming durable pending work.
    ///
    /// Hosts whose transient execution prerequisites are restored only after
    /// startup can call this method, restore those prerequisites, and then
    /// call [`Self::wake`] for the affected sessions.
    pub async fn recover_deferred(self: &Arc<Self>) -> Result<RecoveryReport, ManagedError> {
        self.recover_inner(false).await
    }

    async fn recover_inner(
        self: &Arc<Self>,
        wake_pending: bool,
    ) -> Result<RecoveryReport, ManagedError> {
        let recoveries = self
            .store_call(ManagedPhase::Recover, "", None, |store| store.reconcile())
            .await?;
        let mut report = RecoveryReport::default();
        let mut affected = HashSet::new();
        for recovery in recoveries {
            match recovery {
                ManagedRecovery::Requeued { run, submission } => {
                    report.requeued += 1;
                    affected.insert(run.session_id.clone());
                    self.observe(ManagedEvent::Recovered {
                        session_id: run.session_id.clone(),
                        run_id: run.run_id,
                        submission: Some(submission),
                        kind: RecoveryKind::Requeued,
                    });
                }
                ManagedRecovery::Interrupted { run, submission } => {
                    report.interrupted += 1;
                    affected.insert(run.session_id.clone());
                    self.observe(ManagedEvent::Recovered {
                        session_id: run.session_id.clone(),
                        run_id: run.run_id,
                        submission: Some(submission),
                        kind: RecoveryKind::Interrupted,
                    });
                }
                ManagedRecovery::Released { session_id, run_id } => {
                    report.released += 1;
                    affected.insert(session_id.clone());
                    self.observe(ManagedEvent::Recovered {
                        session_id,
                        run_id,
                        submission: None,
                        kind: RecoveryKind::Released,
                    });
                }
            }
        }
        for session_id in affected {
            self.emit_queue_changed_best_effort(&session_id).await;
        }

        let pending_sessions = self
            .store_call(ManagedPhase::Recover, "", None, |store| {
                store.pending_sessions()
            })
            .await?;
        report.pending_sessions = pending_sessions.len();
        if wake_pending {
            for session_id in pending_sessions {
                self.wake(session_id);
            }
        }
        Ok(report)
    }

    async fn kick_once(
        self: &Arc<Self>,
        session_id: &str,
    ) -> Result<Option<RunContext>, ManagedError> {
        let gate = self.gate(session_id);
        let _pass = gate.lock().await;
        let lifecycle = self.lifecycle.read().await;
        if *lifecycle == ManagedLifecycle::Quiescing {
            return Ok(None);
        }
        // A retained in-memory entry is also a claim fence. In the ordinary
        // path the durable run lease proves the same fact, but after an
        // uncertain requeue/settle result the store may already have released
        // that lease. Consulting only the store would let a later wake claim a
        // successor and overwrite the very fence retained for safety.
        if self
            .active
            .lock()
            .expect("managed active-run map poisoned")
            .contains_key(session_id)
        {
            return Ok(None);
        }
        let proposed_run_id = uuid::Uuid::new_v4().to_string();
        let session = session_id.to_string();
        let claim = self
            .store_call(ManagedPhase::Claim, session_id, None, move |store| {
                store.claim_next(&session, &proposed_run_id)
            })
            .await?;
        let ManagedClaim::Claimed { run, submission } = claim else {
            return Ok(None);
        };
        let submission = *submission;
        let cancel = CancellationToken::new();
        let collision = {
            let mut active = self.active.lock().expect("managed active-run map poisoned");
            match active.entry(run.session_id.clone()) {
                Entry::Vacant(entry) => {
                    entry.insert(ActiveRun {
                        run_id: run.run_id.clone(),
                        kind: ActiveRunKind::Managed {
                            cancel: cancel.clone(),
                        },
                    });
                    false
                }
                Entry::Occupied(_) => true,
            }
        };
        if collision {
            return Err(ManagedError {
                phase: ManagedPhase::Claim,
                session_id: run.session_id,
                submission_id: Some(run.submission_id),
                source: Box::new(InvariantError(
                    "managed claim collided with an in-memory active fence".to_string(),
                )),
            });
        }

        // Register and spawn under the lifecycle read gate, then release that
        // gate before any host projection. The launch barrier preserves
        // lifecycle event order without letting a slow observer prevent
        // `begin_quiesce` from closing claims.
        let (launch, launched_barrier) = oneshot::channel();
        let this = self.clone();
        let launched = run.clone();
        let submission_for_event = submission.clone();
        tokio::spawn(async move {
            this.drive(run, submission, cancel, launched_barrier).await;
        });
        drop(lifecycle);
        self.emit_queue_changed_best_effort(session_id).await;
        self.observe(ManagedEvent::RunStarted {
            run: launched.clone(),
            submission: submission_for_event,
        });
        let _ = launch.send(());
        Ok(Some(launched))
    }

    async fn drive(
        self: Arc<Self>,
        run: RunContext,
        mut submission: SubmissionRecord<P>,
        cancel: CancellationToken,
        launched: oneshot::Receiver<()>,
    ) {
        if launched.await.is_err() {
            let message = "managed launch barrier dropped before RunStarted";
            let session_id = run.session_id.clone();
            let gate = self.gate(&session_id);
            let _transition = gate.lock().await;
            let run_for_store = run.clone();
            let requeued = self
                .store_call(
                    ManagedPhase::Requeue,
                    &run.session_id,
                    Some(&run.submission_id),
                    move |store| store.requeue_claim(&run_for_store, message),
                )
                .await;
            drop(_transition);
            match requeued {
                Ok(true) => {
                    self.emit_queue_changed_best_effort(&session_id).await;
                    self.emit_fault(&ManagedError {
                        phase: ManagedPhase::Claim,
                        session_id: run.session_id.clone(),
                        submission_id: Some(run.submission_id.clone()),
                        source: Box::new(InvariantError(message.to_string())),
                    });
                    // The in-memory entry is also the publication fence. Do
                    // not let a successor claim until every event explaining
                    // why this claimed run was returned has been observed.
                    self.clear_active(&run);
                    self.wake(session_id);
                }
                Ok(false) => self.emit_fault(&ManagedError {
                    phase: ManagedPhase::Requeue,
                    session_id: run.session_id,
                    submission_id: Some(run.submission_id),
                    source: Box::new(InvariantError(
                        "launch-aborted claim could not be requeued".to_string(),
                    )),
                }),
                Err(error) => self.emit_fault(&error),
            }
            return;
        }
        let commit = tokio::spawn(
            self.runner
                .clone()
                .commit_input(run.clone(), submission.clone()),
        )
        .await;
        let commit_error = match commit {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error),
            Err(error) => Some(Box::new(error) as BoxError),
        };
        if let Some(error) = commit_error {
            let message = error.to_string();
            let message_for_store = message.clone();
            let session = run.session_id.clone();
            let gate = self.gate(&session);
            let _transition = gate.lock().await;
            let run_for_store = run.clone();
            let requeued = self
                .store_call(
                    ManagedPhase::Requeue,
                    &run.session_id,
                    Some(&run.submission_id),
                    move |store| store.requeue_claim(&run_for_store, &message_for_store),
                )
                .await;
            drop(_transition);
            let requeue_confirmed = match requeued {
                Ok(true) => true,
                Ok(false) => {
                    self.emit_fault(&ManagedError {
                        phase: ManagedPhase::Requeue,
                        session_id: run.session_id.clone(),
                        submission_id: Some(run.submission_id.clone()),
                        source: Box::new(InvariantError(
                            "claimed run could not be requeued after input commit failure"
                                .to_string(),
                        )),
                    });
                    false
                }
                Err(requeue_error) => {
                    self.emit_fault(&requeue_error);
                    false
                }
            };
            if requeue_confirmed {
                submission.state = SubmissionState::Pending;
                submission.run_id = None;
                submission.started_at_ms = None;
                submission.error = Some(message.clone());
                self.observe(ManagedEvent::InputCommitFailed {
                    run: run.clone(),
                    submission,
                    message: message.clone(),
                });
            }
            self.emit_queue_changed_best_effort(&session).await;
            if requeue_confirmed {
                // Keep the active entry as a publication fence even though
                // the durable lease is already gone. Otherwise a concurrent
                // submit/wake can publish the retry's RunStarted before the
                // prior InputCommitFailed event returns.
                self.clear_active(&run);
            }
            self.emit_fault(&ManagedError {
                phase: ManagedPhase::CommitInput,
                session_id: run.session_id,
                submission_id: Some(run.submission_id),
                source: error,
            });
            return;
        }

        let run_for_store = run.clone();
        let marked = self
            .store_call(
                ManagedPhase::MarkInputCommitted,
                &run.session_id,
                Some(&run.submission_id),
                move |store| store.mark_input_committed(&run_for_store),
            )
            .await;
        match marked {
            Ok(true) => {}
            Ok(false) => {
                self.emit_fault(&ManagedError {
                    phase: ManagedPhase::MarkInputCommitted,
                    session_id: run.session_id,
                    submission_id: Some(run.submission_id),
                    source: Box::new(InvariantError(
                        "active claim disappeared before input commit".to_string(),
                    )),
                });
                return;
            }
            Err(error) => {
                self.emit_fault(&error);
                return;
            }
        }
        submission.state = SubmissionState::Running;
        self.observe(ManagedEvent::InputCommitted {
            run: run.clone(),
            submission: submission.clone(),
        });

        let settlement = match tokio::spawn(self.runner.clone().run(
            run.clone(),
            submission.clone(),
            cancel,
        ))
        .await
        {
            Ok(settlement) => settlement,
            Err(error) => RunSettlement::from(RunOutcome::Failed {
                message: Some(format!("managed runner task failed: {error}")),
            }),
        };
        // Queue edits and steer reservation/delivery share this session gate
        // with settlement. Once `steer_pending` has reserved an item, the
        // active run cannot disappear before delivery either rolls back or
        // transfers durable responsibility to settlement.
        let gate = self.gate(&run.session_id);
        let _settlement = gate.lock().await;
        let run_for_store = run.clone();
        let settlement_for_store = settlement.clone();
        let settled = self
            .store_call(
                ManagedPhase::Settle,
                &run.session_id,
                Some(&run.submission_id),
                move |store| store.finish(&run_for_store, &settlement_for_store),
            )
            .await;
        drop(_settlement);
        match settled {
            Ok(stored) if stored.settled => {
                submission.state = match &settlement.outcome {
                    RunOutcome::Completed => SubmissionState::Succeeded,
                    RunOutcome::Cancelled => SubmissionState::Cancelled,
                    RunOutcome::Failed { .. } => SubmissionState::Failed,
                };
                submission.finished_at_ms = Some(now_ms());
                submission.error = match &settlement.outcome {
                    RunOutcome::Failed { message } => message.clone(),
                    _ => None,
                };
                self.observe(ManagedEvent::RunSettled {
                    run: run.clone(),
                    submission,
                    outcome: settlement.outcome,
                    steered: stored.steered.into(),
                });
                // Durable settlement releases the store lease first, but the
                // in-memory active entry remains the same-session publication
                // fence until RunSettled has returned from the observer.
                self.clear_active(&run);
                self.wake(run.session_id.clone());
                self.emit_queue_changed_best_effort(&run.session_id).await;
            }
            Ok(_) => self.emit_fault(&ManagedError {
                phase: ManagedPhase::Settle,
                session_id: run.session_id,
                submission_id: Some(run.submission_id),
                source: Box::new(InvariantError(
                    "stale managed run could not settle".to_string(),
                )),
            }),
            Err(error) => self.emit_fault(&error),
        }
    }

    async fn emit_queue_changed_best_effort(&self, session_id: &str) {
        // Mutations may complete concurrently, but snapshots must never be
        // observed out of order for one session. Holding this separate gate
        // across load + observe means a later lifecycle operation can only
        // publish an equal or newer durable snapshot.
        let queue_gate = self.queue_gate(session_id);
        let _ordered = queue_gate.lock().await;
        let session = session_id.to_string();
        match self
            .store_call(ManagedPhase::Snapshot, session_id, None, move |store| {
                store.pending(&session)
            })
            .await
        {
            Ok(pending) => self.observe(ManagedEvent::QueueChanged {
                session_id: session_id.to_string(),
                pending: pending.into(),
            }),
            Err(error) => self.emit_fault(&error),
        }
    }

    fn observe(&self, event: ManagedEvent<P>) {
        // Presentation is advisory. A buggy observer must not unwind across a
        // committed store transition and strand the scheduler.
        let _ = catch_unwind(AssertUnwindSafe(|| self.observer.observe(&event)));
    }

    fn emit_fault(&self, error: &ManagedError) {
        self.observe(ManagedEvent::Fault(self.fault(error)));
    }

    fn fault(&self, error: &ManagedError) -> ManagedFault {
        ManagedFault {
            phase: error.phase,
            session_id: error.session_id.clone(),
            submission_id: error.submission_id.clone(),
            message: error.source.to_string(),
        }
    }

    fn gate(&self, session_id: &str) -> Arc<AsyncMutex<()>> {
        Self::weak_gate(&self.gates, session_id, "managed gate map poisoned")
    }

    fn queue_gate(&self, session_id: &str) -> Arc<AsyncMutex<()>> {
        Self::weak_gate(
            &self.queue_gates,
            session_id,
            "managed queue gate map poisoned",
        )
    }

    fn weak_gate(
        gates: &Mutex<HashMap<String, Weak<AsyncMutex<()>>>>,
        session_id: &str,
        poison_message: &str,
    ) -> Arc<AsyncMutex<()>> {
        let mut gates = gates.lock().expect(poison_message);
        gates.retain(|_, gate| gate.strong_count() > 0);
        if let Some(gate) = gates.get(session_id).and_then(Weak::upgrade) {
            return gate;
        }
        let gate = Arc::new(AsyncMutex::new(()));
        gates.insert(session_id.to_string(), Arc::downgrade(&gate));
        gate
    }

    fn clear_active(&self, run: &RunContext) {
        self.clear_active_id(&run.session_id, &run.run_id);
    }

    fn clear_active_id(&self, session_id: &str, run_id: &str) {
        let mut active = self.active.lock().expect("managed active-run map poisoned");
        let removed = if active
            .get(session_id)
            .is_some_and(|current| current.run_id == run_id)
        {
            active.remove(session_id);
            true
        } else {
            false
        };
        drop(active);
        if removed {
            self.active_changed.notify_waiters();
        }
    }

    async fn store_call<T, F>(
        &self,
        phase: ManagedPhase,
        session_id: &str,
        submission_id: Option<&str>,
        call: F,
    ) -> Result<T, ManagedError>
    where
        T: Send + 'static,
        F: FnOnce(Arc<dyn ManagedStore<P>>) -> Result<T, BoxError> + Send + 'static,
    {
        let store = self.store.clone();
        let result = tokio::task::spawn_blocking(move || call(store))
            .await
            .map_err(|error| ManagedError {
                phase,
                session_id: session_id.to_string(),
                submission_id: submission_id.map(str::to_string),
                source: Box::new(error),
            })?;
        result.map_err(|source| ManagedError {
            phase,
            session_id: session_id.to_string(),
            submission_id: submission_id.map(str::to_string),
            source,
        })
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct InvariantError(String);

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}
