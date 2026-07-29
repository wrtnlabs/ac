use std::marker::PhantomData;
use std::sync::Arc;

use ac_store::{
    ManagedClaim as StoreClaim, ManagedEnqueue, ManagedPendingReorder as SqlitePendingReorder,
    ManagedRecovery as StoreRecovery, ManagedRunAcquire as StoreRunAcquire,
    ManagedSteerCommit as SqliteSteerCommit, ManagedSteerDelivery as SqliteSteerDelivery,
    ManagedSubmissionRecord as StoreRecord, ManagedSubmissionState, SqliteStore, StoreError,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::service::{
    BoxError, DirectRunLease, RunContext, RunOutcome, RunSettlement, Submission, SubmissionRecord,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreAccept<P> {
    pub inserted: bool,
    pub record: SubmissionRecord<P>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagedClaim<P> {
    Claimed {
        run: RunContext,
        submission: Box<SubmissionRecord<P>>,
    },
    Held {
        run_id: String,
    },
    Empty,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagedRecovery<P> {
    Requeued {
        run: RunContext,
        submission: SubmissionRecord<P>,
    },
    Interrupted {
        run: RunContext,
        submission: SubmissionRecord<P>,
    },
    Released {
        session_id: String,
        run_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreDirectRunAcquire {
    Acquired(DirectRunLease),
    Held { run_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorePendingReorder {
    Reordered,
    Unchanged,
    Conflict { current_order: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreSteerReservation<P> {
    Begun(SubmissionRecord<P>),
    AlreadySteering(SubmissionRecord<P>),
    AlreadySteered(SubmissionRecord<P>),
    NotPending(SubmissionRecord<P>),
    Missing,
    RunMismatch { active_run_id: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreSteerDelivery<P> {
    Delivered(SubmissionRecord<P>),
    AlreadyDelivered(SubmissionRecord<P>),
    AlreadySteered(SubmissionRecord<P>),
    NotSteering(SubmissionRecord<P>),
    Missing,
    RunMismatch { active_run_id: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreSettlement<P> {
    pub settled: bool,
    pub steered: Vec<SubmissionRecord<P>>,
}

/// Synchronous persistence contract for the managed scheduler.
///
/// The service invokes these methods through `spawn_blocking`. Implementors
/// must make each method's documented transition atomic and must treat
/// submission ids as idempotency keys.
pub trait ManagedStore<P>: Send + Sync + 'static {
    /// Durably accept an idempotent submission.
    ///
    /// This method is failure-atomic: `Err` MUST mean that no new acceptance
    /// committed. An implementation with a fallible post-commit transport or
    /// callback must resolve that uncertainty before returning.
    fn accept(
        &self,
        session_id: &str,
        submission: &Submission<P>,
    ) -> Result<StoreAccept<P>, BoxError>;

    fn get(
        &self,
        session_id: &str,
        submission_id: &str,
    ) -> Result<Option<SubmissionRecord<P>>, BoxError>;

    fn pending(&self, session_id: &str) -> Result<Vec<SubmissionRecord<P>>, BoxError>;

    fn pending_sessions(&self) -> Result<Vec<String>, BoxError>;

    fn cancel_pending(&self, session_id: &str, submission_id: &str) -> Result<bool, BoxError>;

    /// Failure-atomic compare-and-swap of the complete pending order. `Err`
    /// MUST mean no position changed.
    fn reorder_pending(
        &self,
        session_id: &str,
        expected_order: &[String],
        desired_order: &[String],
    ) -> Result<StorePendingReorder, BoxError>;

    /// Failure-atomically reserve a pending record for the exact active run.
    /// `Err` MUST mean no `pending -> steering` transition committed. A store
    /// that can observe a post-commit delivery fault must read back and return
    /// `Begun`/`AlreadySteering`, never an uncertain error.
    fn begin_steer(
        &self,
        session_id: &str,
        submission_id: &str,
        run_id: &str,
    ) -> Result<StoreSteerReservation<P>, BoxError>;

    /// Failure-atomically confirm that the runner took ownership of a reserved
    /// steer. `Err` MUST mean no `steering -> delivered` transition committed.
    /// A store that can observe a post-commit fault must read back and return
    /// `Delivered`/`AlreadyDelivered`, never an uncertain error.
    fn mark_steer_delivered(
        &self,
        session_id: &str,
        submission_id: &str,
        run_id: &str,
    ) -> Result<StoreSteerDelivery<P>, BoxError>;

    fn rollback_steer(
        &self,
        session_id: &str,
        submission_id: &str,
        run_id: &str,
        error: &str,
    ) -> Result<bool, BoxError>;

    /// Atomically claim the FIFO head using `run_id`.
    ///
    /// This method is failure-atomic: `Err` MUST mean that neither a run lease
    /// nor a `claimed` transition committed. The scheduler can only register
    /// and drive a returned [`ManagedClaim::Claimed`]; a custom store that can
    /// observe a post-commit delivery fault must read back `run_id` and return
    /// the committed claim instead of `Err`.
    fn claim_next(&self, session_id: &str, run_id: &str) -> Result<ManagedClaim<P>, BoxError>;

    /// Failure-atomically acquire the direct-run form of the same per-session
    /// lease used by managed submission claims. `Err` MUST mean no direct
    /// lease committed.
    fn try_acquire_direct_run(
        &self,
        session_id: &str,
        run_id: &str,
    ) -> Result<StoreDirectRunAcquire, BoxError>;

    /// Guardedly release a direct-run lease and atomically resolve every bound
    /// `steering` or `delivered` child. The ordered acknowledgement may name
    /// only delivered children: those become `steered`; every other bound
    /// child returns to `pending`. `steered` preserves acknowledgement order.
    fn release_direct_run(
        &self,
        run: &DirectRunLease,
        settlement: &RunSettlement,
    ) -> Result<StoreSettlement<P>, BoxError>;

    fn mark_input_committed(&self, run: &RunContext) -> Result<bool, BoxError>;

    fn requeue_claim(&self, run: &RunContext, error: &str) -> Result<bool, BoxError>;

    /// Guardedly settle a submission run and atomically resolve its bound
    /// steer children under the same ordered acknowledgement rule as
    /// [`Self::release_direct_run`].
    fn finish(
        &self,
        run: &RunContext,
        settlement: &RunSettlement,
    ) -> Result<StoreSettlement<P>, BoxError>;

    /// Reconcile abandoned leases and atomically restore every `steering` or
    /// `delivered` child of each abandoned run to pending. Recovery has no
    /// host persistence proof, so it cannot commit a steer.
    fn reconcile(&self) -> Result<Vec<ManagedRecovery<P>>, BoxError>;
}

/// Stock durable managed store. `P` is serialized into the opaque payload
/// column and decoded only at this adapter boundary; `ac-store` never inspects
/// consumer fields.
pub struct SqliteManagedStore<P> {
    inner: Arc<SqliteStore>,
    _payload: PhantomData<fn() -> P>,
}

impl<P> SqliteManagedStore<P> {
    pub fn new(inner: Arc<SqliteStore>) -> Self {
        Self {
            inner,
            _payload: PhantomData,
        }
    }

    pub fn inner(&self) -> &Arc<SqliteStore> {
        &self.inner
    }
}

impl<P> ManagedStore<P> for SqliteManagedStore<P>
where
    P: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
{
    fn accept(
        &self,
        session_id: &str,
        submission: &Submission<P>,
    ) -> Result<StoreAccept<P>, BoxError> {
        let payload = serde_json::to_string(&submission.payload)?;
        let accepted =
            self.inner
                .enqueue_managed_submission(session_id, &submission.id, &payload)?;
        let (inserted, record) = match accepted {
            ManagedEnqueue::Inserted(record) => (true, record),
            ManagedEnqueue::Existing(record) => (false, record),
        };
        Ok(StoreAccept {
            inserted,
            record: decode(record)?,
        })
    }

    fn get(
        &self,
        session_id: &str,
        submission_id: &str,
    ) -> Result<Option<SubmissionRecord<P>>, BoxError> {
        self.inner
            .get_managed_submission(session_id, submission_id)?
            .map(decode)
            .transpose()
    }

    fn pending(&self, session_id: &str) -> Result<Vec<SubmissionRecord<P>>, BoxError> {
        self.inner
            .list_pending_managed_submissions(session_id)?
            .into_iter()
            .map(decode)
            .collect()
    }

    fn pending_sessions(&self) -> Result<Vec<String>, BoxError> {
        Ok(self.inner.pending_managed_session_ids()?)
    }

    fn cancel_pending(&self, session_id: &str, submission_id: &str) -> Result<bool, BoxError> {
        Ok(self
            .inner
            .cancel_pending_managed_submission(session_id, submission_id)?)
    }

    fn reorder_pending(
        &self,
        session_id: &str,
        expected_order: &[String],
        desired_order: &[String],
    ) -> Result<StorePendingReorder, BoxError> {
        Ok(
            match self.inner.reorder_pending_managed_submissions(
                session_id,
                expected_order,
                desired_order,
            )? {
                SqlitePendingReorder::Reordered => StorePendingReorder::Reordered,
                SqlitePendingReorder::Unchanged => StorePendingReorder::Unchanged,
                SqlitePendingReorder::Conflict { current_order } => {
                    StorePendingReorder::Conflict { current_order }
                }
            },
        )
    }

    fn mark_steer_delivered(
        &self,
        session_id: &str,
        submission_id: &str,
        run_id: &str,
    ) -> Result<StoreSteerDelivery<P>, BoxError> {
        Ok(
            match self.inner.mark_pending_managed_steer_delivered(
                session_id,
                submission_id,
                run_id,
            )? {
                SqliteSteerDelivery::Delivered(record) => {
                    StoreSteerDelivery::Delivered(decode(record)?)
                }
                SqliteSteerDelivery::AlreadyDelivered(record) => {
                    StoreSteerDelivery::AlreadyDelivered(decode(record)?)
                }
                SqliteSteerDelivery::AlreadySteered(record) => {
                    StoreSteerDelivery::AlreadySteered(decode(record)?)
                }
                SqliteSteerDelivery::NotSteering(record) => {
                    StoreSteerDelivery::NotSteering(decode(record)?)
                }
                SqliteSteerDelivery::Missing => StoreSteerDelivery::Missing,
                SqliteSteerDelivery::RunMismatch { active_run_id } => {
                    StoreSteerDelivery::RunMismatch { active_run_id }
                }
            },
        )
    }

    fn begin_steer(
        &self,
        session_id: &str,
        submission_id: &str,
        run_id: &str,
    ) -> Result<StoreSteerReservation<P>, BoxError> {
        Ok(
            match self
                .inner
                .begin_pending_managed_steer(session_id, submission_id, run_id)?
            {
                SqliteSteerCommit::Begun(record) => StoreSteerReservation::Begun(decode(record)?),
                SqliteSteerCommit::AlreadySteering(record) => {
                    StoreSteerReservation::AlreadySteering(decode(record)?)
                }
                SqliteSteerCommit::AlreadySteered(record) => {
                    StoreSteerReservation::AlreadySteered(decode(record)?)
                }
                SqliteSteerCommit::NotPending(record) => {
                    StoreSteerReservation::NotPending(decode(record)?)
                }
                SqliteSteerCommit::Missing => StoreSteerReservation::Missing,
                SqliteSteerCommit::RunMismatch { active_run_id } => {
                    StoreSteerReservation::RunMismatch { active_run_id }
                }
            },
        )
    }

    fn rollback_steer(
        &self,
        session_id: &str,
        submission_id: &str,
        run_id: &str,
        error: &str,
    ) -> Result<bool, BoxError> {
        Ok(self.inner.rollback_pending_managed_steer(
            session_id,
            submission_id,
            run_id,
            Some(error),
        )?)
    }

    fn claim_next(&self, session_id: &str, run_id: &str) -> Result<ManagedClaim<P>, BoxError> {
        Ok(
            match self.inner.claim_next_managed_submission_checked(
                session_id,
                run_id,
                |record| {
                    serde_json::from_str::<P>(&record.payload)
                        .map(|_| ())
                        .map_err(StoreError::from)
                },
            )? {
                StoreClaim::Claimed { submission, run } => ManagedClaim::Claimed {
                    run: RunContext {
                        session_id: run.session_id,
                        run_id: run.run_id,
                        submission_id: submission.submission_id.clone(),
                    },
                    submission: Box::new(decode(submission)?),
                },
                StoreClaim::Held(run) => ManagedClaim::Held { run_id: run.run_id },
                StoreClaim::Empty => ManagedClaim::Empty,
            },
        )
    }

    fn try_acquire_direct_run(
        &self,
        session_id: &str,
        run_id: &str,
    ) -> Result<StoreDirectRunAcquire, BoxError> {
        Ok(
            match self.inner.try_acquire_managed_run(session_id, run_id)? {
                StoreRunAcquire::Acquired(run) => StoreDirectRunAcquire::Acquired(DirectRunLease {
                    session_id: run.session_id,
                    run_id: run.run_id,
                    started_at_ms: run.started_at_ms,
                }),
                StoreRunAcquire::Held(run) => StoreDirectRunAcquire::Held { run_id: run.run_id },
            },
        )
    }

    fn release_direct_run(
        &self,
        run: &DirectRunLease,
        settlement: &RunSettlement,
    ) -> Result<StoreSettlement<P>, BoxError> {
        let settlement = self.inner.release_managed_run(
            &run.session_id,
            &run.run_id,
            &settlement.committed_steer_ids,
        )?;
        Ok(StoreSettlement {
            settled: settlement.settled,
            steered: settlement
                .steered
                .into_iter()
                .map(decode)
                .collect::<Result<_, _>>()?,
        })
    }

    fn mark_input_committed(&self, run: &RunContext) -> Result<bool, BoxError> {
        Ok(self
            .inner
            .mark_managed_input_committed(&run.session_id, &run.run_id)?)
    }

    fn requeue_claim(&self, run: &RunContext, error: &str) -> Result<bool, BoxError> {
        Ok(self
            .inner
            .requeue_managed_claim(&run.session_id, &run.run_id, Some(error))?)
    }

    fn finish(
        &self,
        run: &RunContext,
        settlement: &RunSettlement,
    ) -> Result<StoreSettlement<P>, BoxError> {
        let (state, error) = match &settlement.outcome {
            RunOutcome::Completed => (ManagedSubmissionState::Succeeded, None),
            RunOutcome::Cancelled => (ManagedSubmissionState::Cancelled, None),
            RunOutcome::Failed { message } => (ManagedSubmissionState::Failed, message.as_deref()),
        };
        let settlement = self.inner.finish_managed_run(
            &run.session_id,
            &run.run_id,
            state,
            error,
            &settlement.committed_steer_ids,
        )?;
        Ok(StoreSettlement {
            settled: settlement.settled,
            steered: settlement
                .steered
                .into_iter()
                .map(decode)
                .collect::<Result<_, _>>()?,
        })
    }

    fn reconcile(&self) -> Result<Vec<ManagedRecovery<P>>, BoxError> {
        // Recovery is called under exclusive host authority. Decode every
        // payload it may report before the store transaction mutates anything;
        // otherwise a schema error after commit would consume the recovery
        // action without returning its report, making the healing event
        // impossible to replay.
        for submission in self.inner.active_managed_submissions()? {
            decode::<P>(submission)?;
        }
        self.inner
            .reconcile_managed_runs()?
            .into_iter()
            .map(|recovery| match recovery {
                StoreRecovery::Requeued { submission, run } => {
                    let run = RunContext {
                        session_id: run.session_id,
                        run_id: run.run_id,
                        submission_id: submission.submission_id.clone(),
                    };
                    Ok(ManagedRecovery::Requeued {
                        run,
                        submission: decode(submission)?,
                    })
                }
                StoreRecovery::Interrupted { submission, run } => {
                    let run = RunContext {
                        session_id: run.session_id,
                        run_id: run.run_id,
                        submission_id: submission.submission_id.clone(),
                    };
                    Ok(ManagedRecovery::Interrupted {
                        run,
                        submission: decode(submission)?,
                    })
                }
                StoreRecovery::Released { run } => Ok(ManagedRecovery::Released {
                    session_id: run.session_id,
                    run_id: run.run_id,
                }),
            })
            .collect()
    }
}

fn decode<P: DeserializeOwned>(record: StoreRecord) -> Result<SubmissionRecord<P>, BoxError> {
    Ok(SubmissionRecord {
        session_id: record.session_id,
        sequence: record.sequence,
        queue_position: record.queue_position,
        submission: Submission {
            id: record.submission_id,
            payload: serde_json::from_str(&record.payload)?,
        },
        state: record.state,
        run_id: record.run_id,
        accepted_at_ms: record.accepted_at_ms,
        started_at_ms: record.started_at_ms,
        finished_at_ms: record.finished_at_ms,
        error: record.error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ac_store::ManagedSubmissionState;

    #[test]
    fn corrupt_payload_is_rejected_before_the_durable_claim() {
        let inner = Arc::new(SqliteStore::open_in_memory().unwrap());
        inner.create_session_with_id("s", None).unwrap();
        inner
            .enqueue_managed_submission("s", "bad", "not-json")
            .unwrap();
        let store = SqliteManagedStore::<String>::new(inner.clone());

        assert!(store.claim_next("s", "r").is_err());
        let record = inner.get_managed_submission("s", "bad").unwrap().unwrap();
        assert_eq!(record.state, ManagedSubmissionState::Pending);
        assert!(inner.active_managed_run("s").unwrap().is_none());
    }

    #[test]
    fn corrupt_active_payload_is_rejected_before_recovery_mutates_the_store() {
        let inner = Arc::new(SqliteStore::open_in_memory().unwrap());
        inner.create_session_with_id("s", None).unwrap();
        inner
            .enqueue_managed_submission("s", "bad", "not-json")
            .unwrap();
        assert!(matches!(
            inner.claim_next_managed_submission("s", "r").unwrap(),
            StoreClaim::Claimed { .. }
        ));
        let store = SqliteManagedStore::<String>::new(inner.clone());

        assert!(store.reconcile().is_err());
        let record = inner.get_managed_submission("s", "bad").unwrap().unwrap();
        assert_eq!(record.state, ManagedSubmissionState::Claimed);
        assert_eq!(inner.active_managed_run("s").unwrap().unwrap().run_id, "r");
    }
}
