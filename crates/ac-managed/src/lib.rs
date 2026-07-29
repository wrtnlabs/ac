//! Durable, backend-authoritative submission scheduling above `ac-runtime`.
//!
//! `ac-runtime` owns one live turn, including mid-turn steering.
//! `ac-managed` owns the optional layer a thin or multi-client host needs
//! above it: acknowledge an opaque submission durably, decide start versus
//! queue on the backend, serialize submission-backed and direct runs behind
//! one publication fence, and recover after a process restart.
//!
//! The crate knows no prompt, provider, model, credential, transport, event
//! envelope, or application concept. A host injects a [`ManagedRunner`] and
//! [`ManagedObserver`]; [`SqliteManagedStore`] persists opaque payloads in
//! `ac-store`.

mod service;
mod sqlite;

pub use ac_store::ManagedSubmissionState as SubmissionState;
pub use service::{
    AcceptReceipt, ActiveRunRef, BoxError, DirectRunAcquire, DirectRunLease, ManagedError,
    ManagedEvent, ManagedFault, ManagedObserver, ManagedPhase, ManagedRunner, ManagedRuns,
    NullObserver, PendingReorder, RecoveryKind, RecoveryReport, RunContext, RunOutcome,
    RunSettlement, SteerDelivery, SteerPending, Submission, SubmissionDisposition,
    SubmissionRecord,
};
pub use sqlite::{
    ManagedClaim, ManagedRecovery, ManagedStore, SqliteManagedStore, StoreAccept,
    StoreDirectRunAcquire, StorePendingReorder, StoreSettlement, StoreSteerDelivery,
    StoreSteerReservation,
};
