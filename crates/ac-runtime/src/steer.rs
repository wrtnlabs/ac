//! Mid-turn input — steering. Implements [docs/ac-queue-steer.md]: input
//! submitted while a turn runs joins that turn, drained at step boundaries as a
//! plain user message, never tearing in-flight computation.
//!
//! The state lives behind an `Arc` so both the running turn (which holds
//! `&mut Session`) and an external [`SteerHandle`] (obtained before the turn
//! starts and used from another task) reach the same pending queue.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::Notify;

/// One item of steered input. Text only in v1; the enum is the extension seam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SteerInput {
    Text(String),
}

impl SteerInput {
    pub fn text(s: impl Into<String>) -> Self {
        SteerInput::Text(s.into())
    }
}

/// The class of a turn, which decides whether it can absorb a steer. A regular
/// conversational turn can; a turn that transforms history (compaction, once
/// built) cannot — new user intent cannot coherently join a transformation in
/// progress ([docs/ac-queue-steer.md] §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnClass {
    Regular,
    /// Reserved for the compaction turn of [docs/ac-compaction.md]; not yet
    /// produced, but the steer gate already refuses it.
    Compaction,
}

/// Why a steer was rejected. `NoActiveTurn` returns the input so the caller's
/// generic path is "try steer; on NoActiveTurn, start a turn with these items"
/// — one code path, no time-of-check race.
#[derive(Debug)]
pub enum SteerError {
    /// No turn is running. Carries the input back to the caller.
    NoActiveTurn(Vec<SteerInput>),
    /// The optimistic-concurrency precondition failed; carries the actual
    /// running turn's identity as data (never as a message string to parse).
    TurnMismatch { expected: String, actual: String },
    /// The running turn's class cannot absorb a steer.
    NotSteerable { class: TurnClass },
    /// The input was empty.
    Empty,
}

impl std::fmt::Display for SteerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SteerError::NoActiveTurn(_) => write!(f, "no active turn to steer"),
            SteerError::TurnMismatch { expected, actual } => {
                write!(f, "expected active turn {expected:?} but found {actual:?}")
            }
            SteerError::NotSteerable { class } => {
                write!(f, "turn class {class:?} is not steerable")
            }
            SteerError::Empty => write!(f, "steer input must not be empty"),
        }
    }
}

impl std::error::Error for SteerError {}

struct ActiveTurn {
    id: String,
    class: TurnClass,
    /// The steer queue `Q` (FIFO).
    pending: Vec<SteerInput>,
}

#[derive(Default)]
struct SteerInner {
    active: Option<ActiveTurn>,
}

/// Session-scoped shared state: the active turn and its pending queue.
pub(crate) struct SteerState {
    inner: Mutex<SteerInner>,
    activity: Notify,
    next_id: AtomicU64,
}

impl SteerState {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(SteerInner::default()),
            activity: Notify::new(),
            next_id: AtomicU64::new(0),
        }
    }

    /// Begin a turn of `class`, minting a fresh session-monotonic identity.
    /// Returns the turn id. A session runs one turn at a time (`run_turn` holds
    /// `&mut self`), so this is only ever called with no turn active.
    pub(crate) fn activate(&self, class: TurnClass) -> String {
        let n = self.next_id.fetch_add(1, Ordering::Relaxed);
        let id = format!("t{n}");
        self.inner.lock().unwrap().active = Some(ActiveTurn {
            id: id.clone(),
            class,
            pending: Vec::new(),
        });
        id
    }

    /// Remove the active turn if it is `id` (idempotent — a no-op if already
    /// cleared, so the turn guard and an explicit end never conflict).
    pub(crate) fn deactivate(&self, id: &str) {
        let mut inner = self.inner.lock().unwrap();
        if inner.active.as_ref().map(|t| t.id.as_str()) == Some(id) {
            inner.active = None;
        }
    }

    /// Take and clear the active turn's pending queue.
    pub(crate) fn take_pending(&self) -> Vec<SteerInput> {
        let mut inner = self.inner.lock().unwrap();
        match inner.active.as_mut() {
            Some(turn) => std::mem::take(&mut turn.pending),
            None => Vec::new(),
        }
    }

    pub(crate) fn has_pending(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.active.as_ref().is_some_and(|t| !t.pending.is_empty())
    }

    /// Atomically end turn `id` iff its queue is empty. Returns whether it
    /// ended. The atomicity closes the terminal race: a steer arriving after a
    /// `false`-yielding pending check but before deactivation is still seen
    /// (queue non-empty → not ended); one arriving after a `true` result finds
    /// no active turn.
    pub(crate) fn end_if_idle(&self, id: &str) -> bool {
        let mut inner = self.inner.lock().unwrap();
        match inner.active.as_ref() {
            Some(turn) if turn.id == id && turn.pending.is_empty() => {
                inner.active = None;
                true
            }
            _ => false,
        }
    }

    fn enqueue(&self, expected: Option<&str>, input: Vec<SteerInput>) -> Result<(), SteerError> {
        if input.is_empty() {
            return Err(SteerError::Empty);
        }
        let mut inner = self.inner.lock().unwrap();
        let Some(turn) = inner.active.as_mut() else {
            return Err(SteerError::NoActiveTurn(input));
        };
        if let Some(exp) = expected
            && turn.id != exp
        {
            return Err(SteerError::TurnMismatch {
                expected: exp.to_string(),
                actual: turn.id.clone(),
            });
        }
        if turn.class != TurnClass::Regular {
            return Err(SteerError::NotSteerable { class: turn.class });
        }
        turn.pending.extend(input);
        drop(inner);
        self.activity.notify_waiters();
        Ok(())
    }

    fn active_id(&self) -> Option<String> {
        self.inner
            .lock()
            .unwrap()
            .active
            .as_ref()
            .map(|t| t.id.clone())
    }
}

/// A clonable handle for submitting mid-turn input to whatever turn is running
/// on the session it came from. Obtain it via `Session::steer_handle` before
/// starting the turn; call [`steer`](Self::steer) from another task while the
/// turn runs.
#[derive(Clone)]
pub struct SteerHandle {
    state: std::sync::Arc<SteerState>,
}

impl SteerHandle {
    pub(crate) fn new(state: std::sync::Arc<SteerState>) -> Self {
        Self { state }
    }

    /// Submit input to the running turn. See [`SteerError`] for the rejection
    /// contract; on success the input is drained into history at the next step
    /// boundary and sampled.
    pub fn steer(&self, input: Vec<SteerInput>) -> Result<(), SteerError> {
        self.state.enqueue(None, input)
    }

    /// Like [`steer`](Self::steer) but only if the running turn's identity is
    /// `expected` — optimistic concurrency for a client that must not steer a
    /// turn other than the one it believes is running.
    pub fn steer_expecting(
        &self,
        expected: &str,
        input: Vec<SteerInput>,
    ) -> Result<(), SteerError> {
        self.state.enqueue(Some(expected), input)
    }

    /// The identity of the running turn, or `None` if the session is idle.
    pub fn active_turn_id(&self) -> Option<String> {
        self.state.active_id()
    }

    /// Whether the running turn has accepted but not-yet-drained steers.
    pub fn has_pending(&self) -> bool {
        self.state.has_pending()
    }

    /// Resolves the next time a steer is accepted. A long-blocking tool MAY
    /// await this to wake early when new user intent arrives.
    pub async fn steered(&self) {
        self.state.activity.notified().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn steer_gate_covers_every_rejection_and_the_happy_path() {
        let state = Arc::new(SteerState::new());
        let handle = SteerHandle::new(state.clone());

        // No active turn: the input is handed back verbatim.
        let err = handle.steer(vec![SteerInput::text("a")]).unwrap_err();
        match err {
            SteerError::NoActiveTurn(items) => assert_eq!(items, vec![SteerInput::text("a")]),
            other => panic!("expected NoActiveTurn, got {other:?}"),
        }

        // Empty input is refused before touching state.
        assert!(matches!(handle.steer(vec![]), Err(SteerError::Empty)));
        assert!(handle.active_turn_id().is_none());

        // A regular turn accepts a steer and queues it.
        let id = state.activate(TurnClass::Regular);
        assert_eq!(handle.active_turn_id().as_deref(), Some(id.as_str()));
        handle.steer(vec![SteerInput::text("x")]).unwrap();
        assert!(state.has_pending());

        // The precondition rejects a mismatched turn id, carrying the actual.
        match handle
            .steer_expecting("t999", vec![SteerInput::text("y")])
            .unwrap_err()
        {
            SteerError::TurnMismatch { expected, actual } => {
                assert_eq!(expected, "t999");
                assert_eq!(actual, id);
            }
            other => panic!("expected TurnMismatch, got {other:?}"),
        }
        // ...and accepts the matching one.
        handle
            .steer_expecting(&id, vec![SteerInput::text("z")])
            .unwrap();

        // draining yields the queued items in order and empties the queue.
        assert_eq!(
            state.take_pending(),
            vec![SteerInput::text("x"), SteerInput::text("z")]
        );
        assert!(!state.has_pending());

        // end_if_idle succeeds now that the queue is empty.
        assert!(state.end_if_idle(&id));
        assert!(handle.active_turn_id().is_none());

        // A compaction turn refuses steers.
        let cid = state.activate(TurnClass::Compaction);
        match handle.steer(vec![SteerInput::text("q")]).unwrap_err() {
            SteerError::NotSteerable { class } => assert_eq!(class, TurnClass::Compaction),
            other => panic!("expected NotSteerable, got {other:?}"),
        }
        assert!(!state.has_pending());
        state.deactivate(&cid);
    }

    #[test]
    fn end_if_idle_refuses_while_the_queue_is_non_empty() {
        let state = SteerState::new();
        let id = state.activate(TurnClass::Regular);
        SteerHandle::new(Arc::new(SteerState::new())); // unrelated handle compiles
        state
            .inner
            .lock()
            .unwrap()
            .active
            .as_mut()
            .unwrap()
            .pending
            .push(SteerInput::text("pending"));
        assert!(!state.end_if_idle(&id), "must not end with input pending");
        assert_eq!(state.take_pending().len(), 1);
        assert!(state.end_if_idle(&id));
    }
}
