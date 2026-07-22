//! Shared model-facing markers — text the kit records into history whose exact
//! wording is a cross-crate contract (the runtime records it on cancellation;
//! a fork records it at a ragged edge). Kept here in the zero-dep foundation so
//! both producers reference one string.

/// Recorded into history when a turn is cut on purpose — deliberate
/// cancellation ([docs/ac-queue-steer.md] §5) or a fork whose prefix ends
/// mid-turn ([docs/ac-fork.md] I6). It tells the next turn's model the cut was
/// intentional (not an anomaly to re-attempt) and that partial effects may have
/// landed.
pub const INTERRUPTION_MARKER: &str = "The previous turn was interrupted on purpose. Any commands or tools it had started may \
     have partially executed; do not assume its work completed.";
